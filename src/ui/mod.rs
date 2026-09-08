//! The three panes and the view model they render.

pub mod detail;
pub mod discover;
pub mod list;
pub mod model;
pub mod settings;
pub mod sidebar;

use std::sync::Mutex;

use gpui_kit::component::notification::Notification;
use gpui_kit::component::{Icon, IconName, InteractiveElementExt as _, WindowExt as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Context, Div, InteractiveElement as _, IntoElement, MouseButton, Pixels, Render,
    SharedString, Stateful, Window, WindowControlArea, div, px,
};
use skillbase_core::{
    AgentDef, CacheWriteError, FetchError, GitHub, GitHubError, InstallError, InstallOptions,
    Installed, Installer, Outcome, RemoteCache, Roots, SkillLocation, UreqHttp,
    github_token_source, install_from_github,
};

use crate::ui::model::{display_path, in_words};

/// The reading measure for a line of prose in the detail pane: a description,
/// an explanatory caption, a tooltip's backing text. Wider than this and a
/// line of sentences gets hard to track back to its start.
pub(crate) const PROSE_MAX_WIDTH: f32 = 576.;

/// The width of the Settings and Discover page columns. Those views have no
/// list column to bound them, and what they hold is rows and controls rather
/// than prose, so they get a wider column than the reading measure.
pub(crate) const PAGE_MAX_WIDTH: f32 = 768.;

/// The narrowest the detail pane may be dragged.
///
/// Without it the resizable falls back to the framework's 100px default, and
/// at the 860px minimum window with the list at its widest the pane reaches
/// about 160px. Its header's trailing group — Reveal, Delete, sometimes
/// Update, and the Save commit — needs roughly 200px on its own, and none of
/// it is `flex_shrink_0`, so Save is what gets squeezed out. 360 leaves that
/// group intact with room for the skill name beside it, and still lets the
/// list keep its own minimum in the smallest supported window.
pub(crate) const DETAIL_MIN_WIDTH: f32 = 360.;

/// The height of the first header band in every column.
///
/// The three columns share it so their headers form one band across the top of
/// the window, and the macOS traffic lights are centred in it by
/// `traffic_light_position` in `main`.
pub(crate) const BAND_HEIGHT: Pixels = px(48.);

/// Room the leftmost band leaves for the macOS traffic lights. Matches
/// gpui-kit's `TitleBar` inset, so a `drag_band` can stand in for it without
/// the 34px default height that `TitleBar` otherwise paints first.
#[cfg(target_os = "macos")]
pub(crate) const TRAFFIC_LIGHT_INSET: f32 = 80.;
#[cfg(not(target_os = "macos"))]
pub(crate) const TRAFFIC_LIGHT_INSET: f32 = 12.;

/// A header band the window can be dragged by.
///
/// Only the band that is a `TitleBar` gets that from the component, and the
/// window should move from anywhere along the top. This is the gesture the
/// component implements: a press arms the move and the first movement while it
/// is armed hands the drag to the platform, so a press that does not move still
/// reaches the buttons in the band.
pub fn drag_band(id: &'static str, window: &mut Window, cx: &mut App) -> Stateful<Div> {
    let state = window.use_keyed_state(SharedString::from(format!("{id}-drag")), cx, |_, _| {
        DragBand { should_move: false }
    });

    h_flex()
        .id(id)
        .window_control_area(WindowControlArea::Drag)
        .when(cfg!(target_os = "macos"), |this| {
            this.on_double_click(|_, window, _| window.titlebar_double_click())
        })
        .when(cfg!(target_os = "linux"), |this| {
            this.on_double_click(|_, window, _| window.zoom_window())
        })
        .on_mouse_down_out(window.listener_for(&state, |state, _, _, _| {
            state.should_move = false;
        }))
        .on_mouse_down(
            MouseButton::Left,
            window.listener_for(&state, |state, _, _, _| {
                state.should_move = true;
            }),
        )
        .on_mouse_up(
            MouseButton::Left,
            window.listener_for(&state, |state, _, _, _| {
                state.should_move = false;
            }),
        )
        .on_mouse_move(window.listener_for(&state, |state, _, window, _| {
            if state.should_move {
                state.should_move = false;
                window.start_window_move();
            }
        }))
}

/// Whether the pointer went down on a [`drag_band`] and has not come up yet.
struct DragBand {
    should_move: bool,
}

impl Render for DragBand {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// Agents that ship a brand mark under `assets/icons/agents`.
///
/// Listed rather than derived from the registry so that adding an agent
/// without adding its mark falls back to a generic glyph instead of asking the
/// renderer for a file that is not there, which paints nothing at all.
const MARKED: [&str; 14] = [
    "claude-code",
    "codex",
    "cursor",
    "gemini-cli",
    "opencode",
    "goose",
    "amp",
    "copilot",
    "zed",
    "cline",
    "junie",
    "warp",
    "kiro",
    "devin",
];

/// An agent's own logo, so a row is recognisable before it is read.
///
/// The marks are monochrome by necessity: GPUI rasterises an SVG to an alpha
/// mask and tints it with one colour, so a logo that depends on more than one
/// hue would arrive as a silhouette. Every mark here is drawn to read as one.
pub fn agent_icon(agent: &'static AgentDef) -> Icon {
    if MARKED.contains(&agent.id) {
        Icon::empty().path(format!("icons/agents/{}.svg", agent.id))
    } else {
        Icon::new(IconName::Bot)
    }
}

/// Report what an operation did, or why it did nothing.
///
/// An `InstallError` is a refusal the user has to see — `NotALink` means
/// Skillbase found a real directory where it expected its own symlink, and
/// swallowing that would leave the interface asserting something the disk does
/// not back up. Returns true when the filesystem changed, which includes an
/// operation that stopped part-way: six links removed and then a refusal is
/// still six links removed, and the list has to be read again.
///
/// `title` and `failed` are separate because the title is the part read at a
/// glance: a refusal announced as "Deleted" says the opposite of what happened.
pub fn report(
    title: &str,
    failed: &str,
    result: Result<Outcome, InstallError>,
    roots: &Roots,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    match result {
        Ok(outcome) if outcome.is_noop() => {
            window.push_notification(
                Notification::info(outcome.describe_under(roots.home())).title(title.to_string()),
                cx,
            );
            false
        }
        Ok(outcome) => {
            window.push_notification(
                Notification::success(outcome.describe_under(roots.home()))
                    .title(title.to_string()),
                cx,
            );
            true
        }
        Err(error) => {
            // An operation that stopped part-way carries what it had already
            // done, and `describe_under` prints that above the sentence saying
            // why it stopped. Deleting a skill from nine places and failing on
            // the seventh has to read as six links gone, not as a refusal.
            let changed = error.completed().is_some_and(|done| !done.is_noop());
            window.push_notification(
                // A success says what already happened, so losing it costs
                // nothing. A refusal is the only account of why the disk did
                // not change, and the five seconds a notification otherwise
                // gets is long enough to look away from. It stays until it is
                // dismissed.
                Notification::error(error.describe_under(roots.home()))
                    .title(failed.to_string())
                    .autohide(false),
                cx,
            );
            changed
        }
    }
}

/// Download one skill from GitHub and write it into the store.
///
/// Blocking, and it makes network requests, so every caller runs it on a
/// background task. The remote cache is read around the install and written
/// back, because the digest recorded there is what later tells an edited skill
/// from an untouched one — without it every update would have to ask whether
/// it was about to discard somebody's work and get no answer.
pub fn install_skill(
    roots: &Roots,
    location: &SkillLocation,
    options: &InstallOptions,
) -> Result<Installed, FetchError> {
    let github = GitHub::from_env(UreqHttp::new());
    let mut cache = RemoteCache::read(roots);
    let installed = install_from_github(
        &Installer::new(roots.clone()),
        &github,
        location,
        options,
        &mut cache,
    );
    // Written even when the install failed: the ref and tree lookups that got
    // that far are still worth keeping out of the next check's budget.
    remember_cache_failure(cache.write(roots));
    installed
}

/// Whether the last install-record write failed, and why, waiting for a window
/// to say so.
///
/// [`install_skill`] runs on a background task, where there is no window to
/// push a notification through, and it returns the install's own result, which
/// every caller matches on. So the outcome of the cache write is left here and
/// the report that follows picks it up. A write that succeeds clears the slot,
/// so what is here is always the most recent attempt rather than a failure from
/// some earlier one.
static CACHE_FAILURE: Mutex<Option<String>> = Mutex::new(None);

/// Records what [`RemoteCache::write`] said, for [`take_cache_failure`].
fn remember_cache_failure(error: Option<CacheWriteError>) {
    if let Ok(mut slot) = CACHE_FAILURE.lock() {
        *slot = error.map(|error| error.to_string());
    }
}

/// Why the last install record could not be written, taken so that one failure
/// is reported once.
pub fn take_cache_failure() -> Option<String> {
    CACHE_FAILURE.lock().ok().and_then(|mut slot| slot.take())
}

/// Whether the cache write that follows a delete failed, and why, waiting for
/// a window to say so.
///
/// A delete drops the records of the skills it removed, and it runs on a
/// background task like an install does, so its failure needs the same kind of
/// slot. It gets its own rather than sharing [`CACHE_FAILURE`] because the two
/// can be in flight at once: an install waits on the network and a delete does
/// not, so a delete started during an install finishes first, and one slot
/// would have it report the install's failure as its own — and leave the
/// install's failure unreported, since each failure is taken once.
static DELETE_CACHE_FAILURE: Mutex<Option<String>> = Mutex::new(None);

/// Records what [`RemoteCache::write`] said after a delete, for
/// [`take_delete_cache_failure`].
pub fn remember_delete_cache_failure(error: Option<CacheWriteError>) {
    if let Ok(mut slot) = DELETE_CACHE_FAILURE.lock() {
        *slot = error.map(|error| error.to_string());
    }
}

/// Why the deleted skills' install records could not be dropped, taken so that
/// one failure is reported once.
pub fn take_delete_cache_failure() -> Option<String> {
    DELETE_CACHE_FAILURE
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
}

/// The warning for a download whose install record could not be written.
///
/// The download worked, so this cannot be an error; but nothing else in the
/// interface will ever say why the skill reports no install record, so it is
/// not something to leave out either.
///
/// `lead` opens the sentence, because a single install can say that the skill
/// is installed and a batch cannot. The reason and what follows from it are the
/// same either way.
pub fn cache_failure_notification(lead: &str, reason: &str) -> Notification {
    Notification::warning(format!(
        "{lead}: {reason}. Without that record Skillbase cannot tell an edited copy from an \
         untouched one, so it asks before replacing one, and the next update check spends \
         GitHub's whole hourly budget again."
    ))
    .title("Install record not written")
    .autohide(false)
}

/// The warning for a delete whose install record could not be dropped.
///
/// The skill is gone from the disk and its record is not, which is the staleness
/// the delete was meant to prevent: the records are keyed by skill name, so the
/// one left behind answers for whatever takes that name next.
///
/// `lead` opens the sentence, because a single delete can name one record and a
/// batch cannot. What follows from it is the same either way.
pub fn delete_cache_failure_notification(lead: &str, reason: &str) -> Notification {
    Notification::warning(format!(
        "{lead}: {reason}. Install records are keyed by skill name, so the next skill to take \
         that name inherits this one. Skillbase then measures it against the deleted skill's \
         install-time contents, and its See what changed link opens the deleted skill's \
         repository."
    ))
    .title("Install record not removed")
    .autohide(false)
}

/// The same sentence with an upper-case first letter, so an error written to
/// read inside a sentence can start one.
pub fn capitalized(sentence: &str) -> String {
    let mut chars = sentence.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Report what an install or an update did, and hand back the name it landed
/// under so the caller can scan again and select it.
///
/// A [`FetchError`] is the one thing on this path the user cannot see for
/// themselves: nothing appears in the list, and without a sentence there is
/// nothing to say why.
///
/// Two things can go wrong around an install that still leaves the skill
/// installed, and each gets its own notification beside the success: the
/// install record could not be written, and a part-downloaded skill from an
/// earlier run could not be cleared out of the staging directory.
pub fn report_install(
    title: &str,
    failed: &str,
    result: Result<Installed, FetchError>,
    roots: &Roots,
    window: &mut Window,
    cx: &mut App,
) -> Option<SharedString> {
    match result {
        Ok(installed) => {
            let name = SharedString::from(installed.name.clone());
            window.push_notification(
                Notification::success(format!(
                    "Wrote {} file{} to {}",
                    installed.files,
                    if installed.files == 1 { "" } else { "s" },
                    display_path(&installed.dir, roots)
                ))
                .title(title.to_string()),
                cx,
            );
            if let Some(reason) = take_cache_failure() {
                window.push_notification(
                    cache_failure_notification(
                        "The skill was installed, but what it installed could not be recorded",
                        &reason,
                    ),
                    cx,
                );
            }
            if let Some(warning) = installed.staging.warning() {
                window.push_notification(
                    Notification::warning(warning)
                        .title("Staging directory not cleared")
                        .autohide(false),
                    cx,
                );
            }
            Some(name)
        }
        Err(error) => {
            window.push_notification(
                // Nothing appeared in the list, so this sentence is the whole
                // account of what went wrong. It stays until it is dismissed.
                Notification::error(fetch_sentence(&error, roots))
                    .title(failed.to_string())
                    .autohide(false),
                cx,
            );
            None
        }
    }
}

/// Why a fetch failed, in a sentence the user can act on.
///
/// Two of GitHub's refusals arrive as facts rather than as anything a person
/// can do: a rate limit carries a Unix timestamp, and a 404 carries the same
/// text whether the repository is private, misspelled or on another branch.
/// Both are rewritten here. Everything else already reads as a sentence and is
/// passed through.
fn fetch_sentence(error: &FetchError, roots: &Roots) -> String {
    match error {
        FetchError::GitHub(GitHubError::RateLimited { reset, .. }) => rate_limit_sentence(*reset),
        FetchError::GitHub(GitHubError::NotFound { what }) => not_found_sentence(what),
        // A refusal from the installer writes its own paths, and so does a
        // partial install: the trash directory it names is what the user has
        // to go and find. Both get the same `~` the rest of the interface
        // uses.
        FetchError::Install(_) | FetchError::Partial { .. } => error.describe_under(roots.home()),
        other => other.to_string(),
    }
}

/// When GitHub will answer again, and what stops it happening so soon.
///
/// The same sentence the update path writes, worked out from the reset time
/// GitHub sent with the refusal. Named as a wait rather than as a clock time:
/// the reader wants to know how long, and "in 43 minutes" answers that without
/// a timezone.
fn rate_limit_sentence(reset_unix: i64) -> String {
    let wait = u64::try_from(reset_unix)
        .ok()
        .and_then(|reset| std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(reset)))
        .and_then(|reset| reset.duration_since(std::time::SystemTime::now()).ok())
        .map(|wait| wait.as_secs());

    let mut sentence = match wait {
        Some(seconds) => format!(
            "GitHub's request limit is spent. It resets in {}.",
            in_words(seconds)
        ),
        None => "GitHub's request limit is spent.".to_string(),
    };
    if !github_token_source().has_token() {
        sentence.push_str(
            " Without a token GitHub allows 60 requests an hour. Set SKILLBASE_GITHUB_TOKEN, \
             or log in with gh, to raise that to 5000.",
        );
    }
    sentence
}

/// The three things a GitHub 404 can mean, said out loud.
///
/// GitHub answers 404 for a repository that exists but is private, so the raw
/// message sends someone hunting for a typo they do not have.
fn not_found_sentence(what: &str) -> String {
    let mut sentence = format!(
        "GitHub has nothing at {what}. Check the owner, the repository and the branch: a branch \
         named something other than main has to be written out."
    );
    if !github_token_source().has_token() {
        sentence.push_str(
            " A private repository answers the same way. Set SKILLBASE_GITHUB_TOKEN, or log in \
             with gh, so Skillbase can read one.",
        );
    } else {
        sentence.push_str(" If the repository is private, check that your token can read it.");
    }
    sentence
}
