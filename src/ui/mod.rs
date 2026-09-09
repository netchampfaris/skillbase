//! The three panes and the view model they render.

pub mod detail;
pub mod discover;
pub mod list;
pub mod model;
pub mod settings;
pub mod sidebar;

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, InteractiveElementExt as _, Sizable as _, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Anchor, AnyElement, App, AppContext as _, Context, Div, Global, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, Pixels, Render, ScrollHandle, SharedString,
    Stateful, StatefulInteractiveElement as _, Styled as _, Window, WindowControlArea, div, px,
};
use skillbase_core::{
    AgentDef, CacheWriteError, Change, FetchError, GitHub, GitHubError, InstallError,
    InstallOptions, Installed, Installer, Outcome, RemoteCache, Restored, Roots, SkillLocation,
    UreqHttp, github_token_source, install_from_github,
};

use crate::ui::model::{display_path, in_words, join_and};

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

// ---------------------------------------------------------------- notices

/// A message that stays on screen until the reader clears it.
///
/// Most of what Skillbase says is a toast: it names something the user can see
/// for themselves and goes after five seconds. A notice is the other kind —
/// the only account of something that did not happen, or an offer the user has
/// to be given time to take. Nothing else on screen records that an install
/// failed, so the sentence cannot vanish on a timer.
///
/// Notices do not each get their own toast. Every outstanding one is a row in
/// a single card, so a second failure lands under the first rather than on top
/// of it, and each row carries the button that clears it. The card sits in the
/// bottom-right corner, away from the toolbar and the Save button in the top
/// right of the content area, and away from the toasts that report what did
/// work.
#[derive(Clone)]
pub struct Notice {
    kind: NoticeKind,
    title: SharedString,
    message: SharedString,
    action: Option<NoticeAction>,
}

/// How serious a [`Notice`] is, which is the icon it gets.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NoticeKind {
    Error,
    Warning,
    Info,
}

/// Something to run later, from a button on a notice or once an undo has
/// finished. Shared rather than owned because a notice is rebuilt on every
/// frame it is on screen.
type Callback = Rc<dyn Fn(&mut Window, &mut App)>;

/// The one thing a notice offers to do about itself: undo a delete, link a
/// skill into the agents that cannot see it.
#[derive(Clone)]
struct NoticeAction {
    label: SharedString,
    run: Callback,
}

impl Notice {
    /// Something did not happen, and this is the only account of why.
    pub fn error(title: impl Into<SharedString>, message: impl Into<SharedString>) -> Self {
        Self::new(NoticeKind::Error, title, message)
    }

    /// Something happened, and left a state the user would want to know about.
    pub fn warning(title: impl Into<SharedString>, message: impl Into<SharedString>) -> Self {
        Self::new(NoticeKind::Warning, title, message)
    }

    /// Something happened, and there is one more thing worth doing about it.
    pub fn info(title: impl Into<SharedString>, message: impl Into<SharedString>) -> Self {
        Self::new(NoticeKind::Info, title, message)
    }

    fn new(
        kind: NoticeKind,
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
    ) -> Self {
        Self {
            kind,
            title: title.into(),
            message: message.into(),
            action: None,
        }
    }

    /// Offer one thing to do about it. Taking the offer clears the notice.
    pub fn action(
        mut self,
        label: impl Into<SharedString>,
        run: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        self.action = Some(NoticeAction {
            label: label.into(),
            run: Rc::new(run),
        });
        self
    }

    /// The icon this kind of notice carries, in the colour the theme gives it.
    fn icon(&self, cx: &App) -> Icon {
        match self.kind {
            NoticeKind::Error => Icon::new(IconName::CircleX).text_color(cx.theme().danger),
            NoticeKind::Warning => {
                Icon::new(IconName::TriangleAlert).text_color(cx.theme().warning)
            }
            NoticeKind::Info => Icon::new(IconName::Info).text_color(cx.theme().info),
        }
    }
}

/// Every notice that has not been cleared, oldest first.
///
/// Application-global rather than held by a view: a notice is pushed from
/// wherever the operation finished — a background task's completion, a
/// notification's own button — and every one of those has an `App` and a
/// `Window` and nothing else.
#[derive(Default)]
struct Notices {
    /// The outstanding notices, each with the key its buttons are identified
    /// by. Keys are never reused, so a row's Dismiss cannot come to mean a
    /// different row after the one above it goes.
    items: Vec<(u64, Notice)>,
    /// The last key handed out.
    last_key: u64,
    /// Where the card is scrolled to, kept across frames so a redraw does not
    /// throw the reader back to the top.
    scroll: ScrollHandle,
}

impl Global for Notices {}

/// The notification the notices are rendered in. Pushing again under the same
/// id replaces the card rather than stacking a second one behind it.
struct NoticeCard;

/// How many notices are kept. Past this the oldest goes: a card holding
/// twenty unread failures is not read either, and the newest are the ones the
/// user is still in a position to act on.
const MAX_NOTICES: usize = 12;

/// The tallest the card gets before its rows scroll inside it.
///
/// Four failures in a row, each two sentences long, would otherwise make a
/// card taller than the window.
const NOTICES_MAX_HEIGHT: f32 = 340.;

/// Put a message on screen that stays until it is cleared.
pub fn push_notice(notice: Notice, window: &mut Window, cx: &mut App) {
    let notices = cx.default_global::<Notices>();
    notices.last_key += 1;
    let key = notices.last_key;
    notices.items.push((key, notice));
    let overflow = notices.items.len().saturating_sub(MAX_NOTICES);
    notices.items.drain(..overflow);
    // The newest row is the one the reader is waiting on, and it is the last
    // one. Past the height cap it would otherwise arrive below the fold.
    notices.scroll.scroll_to_bottom();

    window.push_notification(
        // No title, no message and no type of its own: everything the card
        // shows is a row, and a row carries its own icon and heading. The card
        // is pushed again for every notice, which replaces the one already up
        // with a card holding the new row as well.
        Notification::new()
            .id::<NoticeCard>()
            .autohide(false)
            .placement(Anchor::BottomRight)
            .content(|_, _, cx| render_notices(cx))
            // The card's own close button, and a middle-click on it, clear
            // every row at once. Without this the rows would come back the
            // next time anything pushed a notice.
            .on_close(|_, cx| {
                cx.default_global::<Notices>().items.clear();
            }),
        cx,
    );
}

/// Every outstanding notice as a row, newest last.
fn render_notices(cx: &mut Context<Notification>) -> AnyElement {
    let notices = cx.default_global::<Notices>();
    let items = notices.items.clone();
    let scroll = notices.scroll.clone();
    let last = items.len().saturating_sub(1);

    // A column, and one that takes the whole width the card gives it: a plain
    // `div` is a flex row, which would size the rows to their own content
    // rather than to the card. The card is 382px wide and says so itself, so
    // nothing here sets a width of its own.
    v_flex()
        .id("notices")
        .debug_selector(|| "notices".to_string())
        .w_full()
        .gap_3()
        .track_scroll(&scroll)
        .max_h(px(NOTICES_MAX_HEIGHT))
        .overflow_y_scroll()
        .children(items.into_iter().enumerate().map(|(index, (key, notice))| {
            v_flex()
                .w_full()
                .gap_3()
                .child(render_notice(key, &notice, cx))
                // A rule between rows, so two failures do not read as
                // one paragraph.
                .when(index < last, |this| {
                    this.child(div().h(px(1.)).w_full().bg(cx.theme().border))
                })
        }))
        .into_any_element()
}

/// One notice: its icon, what happened, and the buttons that answer it.
fn render_notice(key: u64, notice: &Notice, cx: &mut Context<Notification>) -> AnyElement {
    let action = notice.action.clone();

    h_flex()
        .w_full()
        .items_start()
        .gap_2()
        // The icon and the dismiss button keep their size and their place at
        // the top of the row: the message between them is what grows.
        .child(div().flex_shrink_0().pt(px(2.)).child(notice.icon(cx)))
        .child(
            v_flex()
                .flex_1()
                // Allowed to be narrower than the sentence it holds. Without
                // this the column cannot shrink below the width of the whole
                // unwrapped message — a flex item is at least its own
                // minimum content width, and an unwrapped line's is the line —
                // so the message was laid out at its natural width and cut
                // off wherever the card ended.
                .min_w_0()
                .gap_1()
                .child(div().text_sm().font_semibold().child(notice.title.clone()))
                .child(
                    div()
                        .debug_selector(|| "notice-message".to_string())
                        .text_sm()
                        .child(notice.message.clone()),
                )
                .when_some(action, |this, action| {
                    let run = action.run.clone();
                    this.child(
                        h_flex().pt_1().justify_end().child(
                            Button::new(("notice-action", key as usize))
                                .debug_selector(|| "notice-action".to_string())
                                .primary()
                                .small()
                                .label(action.label.clone())
                                .on_click(cx.listener(move |card, _, window, cx| {
                                    // The offer is taken, so the row that made
                                    // it has nothing left to say.
                                    clear_notice(key, card, window, cx);
                                    run(window, cx);
                                })),
                        ),
                    )
                }),
        )
        // Always drawn, rather than appearing on hover the way the card's own
        // close button does: a notice that never goes on its own has to be
        // clearable without hunting for the control that clears it.
        .child(
            Button::new(("dismiss-notice", key as usize))
                .debug_selector(|| "dismiss-notice".to_string())
                .icon(IconName::Close)
                .ghost()
                .xsmall()
                .accessibility_label("Dismiss this message")
                .on_click(cx.listener(move |card, _, window, cx| {
                    cx.stop_propagation();
                    clear_notice(key, card, window, cx);
                })),
        )
        .into_any_element()
}

/// Take one notice off the card, and take the card away with the last of them.
fn clear_notice(
    key: u64,
    card: &mut Notification,
    window: &mut Window,
    cx: &mut Context<Notification>,
) {
    let notices = cx.default_global::<Notices>();
    notices.items.retain(|(existing, _)| *existing != key);
    let empty = notices.items.is_empty();
    if empty {
        card.dismiss(window, cx);
    } else {
        cx.notify();
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
            // A success says what already happened, so losing it costs
            // nothing. A refusal is the only account of why the disk did not
            // change, and the five seconds a notification otherwise gets is
            // long enough to look away from. It stays until it is cleared.
            push_notice(
                Notice::error(failed, error.describe_under(roots.home())),
                window,
                cx,
            );
            changed
        }
    }
}

/// What a delete does to the disk, said the same way wherever it is confirmed.
///
/// One skill and several skills are the same operation, and the two dialogs
/// used to describe it differently: the one for a single skill said it removed
/// the directory, which sounds permanent, and never mentioned links. Both say
/// this now. `directories` is how many real directories the delete would move,
/// so the sentence counts what is actually going.
pub fn delete_effect(directories: usize) -> String {
    let plural = directories != 1;
    format!(
        "The director{} move{} to ~/.skillbase/trash and {} links are removed. The message that \
         follows offers to put {} back.",
        if plural { "ies" } else { "y" },
        if plural { "" } else { "s" },
        if plural { "their" } else { "its" },
        if plural { "them" } else { "it" },
    )
}

/// Report a delete, and offer to put it back.
///
/// Delete is the one operation here that cannot be corrected by repeating it.
/// The directory is in `~/.skillbase/trash` under a name stamped with the
/// second it landed, and until now getting it back meant leaving Skillbase for
/// the Finder. So the report stays on screen until it is cleared, and carries
/// the button that reverses it.
///
/// A delete that stopped part-way is offered the same button: six links
/// removed and then a refusal is six links to put back.
///
/// `after` re-reads the disk once the undo has run, because nothing else
/// notices that a directory came back.
pub fn report_delete(
    title: &str,
    failed: &str,
    result: Result<Outcome, InstallError>,
    roots: &Roots,
    after: Callback,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let home = roots.home();
    let (notice, changed) = match &result {
        Ok(outcome) if outcome.is_noop() => {
            // Nothing was taken away, so there is nothing to put back and no
            // reason for the sentence to stay.
            window.push_notification(
                Notification::info(outcome.describe_under(home)).title(title.to_string()),
                cx,
            );
            return false;
        }
        Ok(outcome) => (Notice::info(title, outcome.describe_under(home)), true),
        Err(error) => (
            Notice::error(failed, error.describe_under(home)),
            error.completed().is_some_and(|done| !done.is_noop()),
        ),
    };

    let changes = match &result {
        Ok(outcome) => outcome.changes.clone(),
        Err(error) => error
            .completed()
            .map(|done| done.changes.clone())
            .unwrap_or_default(),
    };
    let undoable = changes.iter().any(|change| {
        matches!(
            change,
            Change::MovedToTrash { .. } | Change::RemovedSymlink { .. }
        )
    });

    let notice = if undoable {
        let roots = roots.clone();
        notice.action("Undo", move |window, cx| {
            undo_delete(changes.clone(), roots.clone(), after.clone(), window, cx);
        })
    } else {
        notice
    };
    push_notice(notice, window, cx);
    changed
}

/// Put back what a delete took away, and say how much of it came back.
///
/// On a background task, like the delete itself: putting a skill back is a
/// rename, except across a filesystem boundary, where it is a copy of the
/// whole directory.
fn undo_delete(
    changes: Vec<Change>,
    roots: Roots,
    after: Callback,
    window: &mut Window,
    cx: &mut App,
) {
    let installer_roots = roots.clone();
    window
        .spawn(cx, async move |cx| {
            let restored = cx
                .background_spawn(async move { Installer::new(installer_roots).restore(&changes) })
                .await;
            cx.update(|window, cx| {
                report_restored(&restored, &roots, window, cx);
                after(window, cx);
            })
            .ok();
        })
        .detach();
}

/// What an undo put back, and what it could not.
///
/// A restore that leaves anything out is reported as a message that stays:
/// the skill is on screen again, which reads as though the delete was
/// reversed, and the part that was not is the thing the user has to be told.
fn report_restored(restored: &Restored, roots: &Roots, window: &mut Window, cx: &mut App) {
    let message = restored.describe_under(roots.home());
    if restored.is_noop() {
        push_notice(
            Notice::error("Could not undo the delete", message),
            window,
            cx,
        );
    } else if restored.missed.is_empty() {
        window.push_notification(Notification::success(message).title("Put back"), cx);
    } else {
        push_notice(
            Notice::warning("Part of it could not be put back", message),
            window,
            cx,
        );
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
pub fn cache_failure_notification(lead: &str, reason: &str) -> Notice {
    Notice::warning(
        "Install record not written",
        format!(
            "{lead}: {reason}. Without that record Skillbase cannot tell an edited copy from an \
             untouched one, so it asks before replacing one, and the next update check spends \
             GitHub's whole hourly budget again."
        ),
    )
}

/// The warning for a delete whose install record could not be dropped.
///
/// The skill is gone from the disk and its record is not, which is the staleness
/// the delete was meant to prevent: the records are keyed by skill name, so the
/// one left behind answers for whatever takes that name next.
///
/// `lead` opens the sentence, because a single delete can name one record and a
/// batch cannot. What follows from it is the same either way.
pub fn delete_cache_failure_notification(lead: &str, reason: &str) -> Notice {
    Notice::warning(
        "Install record not removed",
        format!(
            "{lead}: {reason}. Install records are keyed by skill name, so the next skill to \
             take that name inherits this one. Skillbase then measures it against the deleted \
             skill's install-time contents, and its See what changed link opens the deleted \
             skill's repository."
        ),
    )
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
    let installed = install_outcome(failed, result, roots, window, cx)?;
    window.push_notification(
        Notification::success(wrote_sentence(&installed, roots)).title(title.to_string()),
        cx,
    );
    install_warnings(&installed, window, cx);
    Some(installed.name.into())
}

/// Report a failed install, or hand back what a successful one wrote.
///
/// Everything [`report_install`] does apart from the sentence saying it worked.
/// An install started from Discover leaves that sentence to its caller, because
/// what there is to say about a skill that landed depends on which agents can
/// read it, and that is a question about the machine rather than about the
/// download.
pub fn install_outcome(
    failed: &str,
    result: Result<Installed, FetchError>,
    roots: &Roots,
    window: &mut Window,
    cx: &mut App,
) -> Option<Installed> {
    match result {
        Ok(installed) => Some(installed),
        Err(error) => {
            // Nothing appeared in the list, so this sentence is the whole
            // account of what went wrong. It stays until it is cleared.
            push_notice(
                Notice::error(failed, fetch_sentence(&error, roots)),
                window,
                cx,
            );
            None
        }
    }
}

/// What an install wrote, and where.
pub fn wrote_sentence(installed: &Installed, roots: &Roots) -> String {
    format!(
        "Wrote {} file{} to {}.",
        installed.files,
        if installed.files == 1 { "" } else { "s" },
        display_path(&installed.dir, roots)
    )
}

/// The two things that can go wrong around an install that still leaves the
/// skill installed: the install record could not be written, and a
/// part-downloaded skill from an earlier run could not be cleared out of the
/// staging directory. Each gets its own notification beside the success.
pub fn install_warnings(installed: &Installed, window: &mut Window, cx: &mut App) {
    if let Some(reason) = take_cache_failure() {
        push_notice(
            cache_failure_notification(
                "The skill was installed, but what it installed could not be recorded",
                &reason,
            ),
            window,
            cx,
        );
    }
    if let Some(warning) = installed.staging.warning() {
        push_notice(
            Notice::warning("Staging directory not cleared", warning),
            window,
            cx,
        );
    }
}

/// The agents on this machine that cannot see a skill sitting in the store.
///
/// Every install writes into `~/.agents/skills`. Most agents read that
/// directory for themselves; Claude Code and Codex read one of their own, and a
/// skill reaches them only through a link. This is the list of the ones that
/// are on the machine, do not read the store, and hold no link to this skill
/// yet — the answer to "can my agent use it now?".
///
/// [`Roots::agent_present`] decides what is on the machine, so this cannot come
/// to disagree with the sidebar's count or the detail pane's rows.
pub fn cannot_see(roots: &Roots, name: &str) -> Vec<&'static AgentDef> {
    roots
        .present_agents()
        .into_iter()
        .filter(|agent| !agent.reads_shared)
        .filter(|agent| !holds_link(roots, agent, name))
        .collect()
}

/// Whether this agent already holds a link to `name`, active or parked in its
/// disabled directory.
///
/// A parked link is not a link that cannot see the skill; it is one the user
/// switched off, which is a different question and not one an install should
/// reopen.
fn holds_link(roots: &Roots, agent: &'static AgentDef, name: &str) -> bool {
    let there = |dir: PathBuf| std::fs::symlink_metadata(dir.join(name)).is_ok();
    there(roots.agent_dir(agent)) || roots.disabled_dir(agent).is_some_and(there)
}

/// Who can use what was just installed, in one sentence.
///
/// `subject` names what landed, as it reads inside a sentence: "pdf", "these 3
/// skills". `plural` decides the pronoun. `None` when there is nothing worth
/// saying, which is the machine with no agent on it at all: naming every agent
/// that is absent would be a list of software the user does not have.
pub fn reach_sentence(
    roots: &Roots,
    blind: &[&'static AgentDef],
    subject: &str,
    plural: bool,
) -> Option<String> {
    let store = display_path(&roots.store_dir(), roots);
    if blind.is_empty() {
        if roots.present_agents().is_empty() {
            return None;
        }
        return Some(format!(
            "Every agent on this machine reads {store}, so {subject} {} ready to use.",
            if plural { "are" } else { "is" }
        ));
    }
    let names: Vec<&str> = blind.iter().map(|agent| agent.display_name).collect();
    Some(format!(
        "{} {} not read {store}, so {} cannot use {subject} yet.",
        join_and(&names),
        if names.len() == 1 { "does" } else { "do" },
        if names.len() == 1 { "it" } else { "they" }
    ))
}

/// What the button that answers [`reach_sentence`] says.
pub fn link_label(blind: &[&'static AgentDef]) -> String {
    match blind {
        [agent] => format!("Link to {}", agent.display_name),
        many => format!("Link to {} agents", many.len()),
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

#[cfg(test)]
mod notice_tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use gpui_kit::component::Root;
    use gpui_kit::{TestAppContext, VisualTestContext};

    use super::*;
    use crate::ui::list::dialog_probe::window;

    /// How many notification cards the window is showing.
    ///
    /// The count is the point: every outstanding notice is a row in one card,
    /// so a second failure can never draw over the first.
    fn cards(cx: &mut VisualTestContext) -> usize {
        cx.update(|window, cx| {
            Root::read(window, cx)
                .notification
                .read(cx)
                .notifications()
                .len()
        })
    }

    /// The heading of every notice on the card, oldest first.
    fn headings(cx: &mut VisualTestContext) -> Vec<String> {
        cx.update(|_, cx| {
            cx.default_global::<Notices>()
                .items
                .iter()
                .map(|(_, notice)| notice.title.to_string())
                .collect()
        })
    }

    /// Two failures used to draw on top of one another. They are rows in one
    /// card now, and the card draws.
    #[gpui_kit::test]
    fn two_notices_are_two_rows_of_one_card(cx: &mut TestAppContext) {
        let (mut cx, _) = window(cx);
        cx.update(|window, cx| {
            push_notice(
                Notice::error("Could not install", "The repository is too large."),
                window,
                cx,
            );
            push_notice(
                Notice::warning("Nothing was updated", "GitHub answered nothing."),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        assert_eq!(cards(&mut cx), 1, "the two notices took two cards");
        assert_eq!(
            headings(&mut cx),
            ["Could not install", "Nothing was updated"]
        );
        assert!(
            cx.debug_bounds("notices").is_some(),
            "the notices never drew"
        );
    }

    /// A notice carrying a button is built by the same closure, on the frame
    /// after it is pushed. This is the draw that runs it.
    #[gpui_kit::test]
    fn a_notice_with_a_button_draws(cx: &mut TestAppContext) {
        let (mut cx, _) = window(cx);
        cx.update(|window, cx| {
            push_notice(
                Notice::info("Deleted", "moved ~/.agents/skills/alpha to the trash")
                    .action("Undo", |_, _| {}),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        assert_eq!(cards(&mut cx), 1);
        assert!(
            cx.debug_bounds("notices").is_some(),
            "the notice never drew"
        );
    }

    /// The two shapes a long notice comes in: an install that failed, which is
    /// two sentences and a URL, and one that worked, which names a path and
    /// the agents that can see it.
    const TWO_SENTENCES: &str = "GitHub has nothing at \
        https://api.github.com/repos/anthropics/skills. If the repository is \
        private, check that your token can read it.";
    const AFTER_AN_INSTALL: &str = "Wrote 3 files to ~/.agents/skills/ask-matt. \
        Claude Code and Codex can see it.";

    /// A message used to be laid out at its natural width and cut off at the
    /// edge of the card, mid-word. It wraps now, onto as many lines as it
    /// needs, and the card is no wider for it.
    #[gpui_kit::test]
    fn a_long_message_wraps_inside_the_card(cx: &mut TestAppContext) {
        let (mut cx, _) = window(cx);
        cx.update(|window, cx| {
            push_notice(Notice::error("Could not install", "Short."), window, cx);
        });
        cx.run_until_parked();
        let narrow = cx.debug_bounds("notices").expect("the card never drew");
        let one_line = cx
            .debug_bounds("notice-message")
            .expect("the message never drew")
            .size
            .height;
        let dismiss = cx
            .debug_bounds("dismiss-notice")
            .expect("the dismiss button never drew");

        cx.update(|window, cx| {
            cx.default_global::<Notices>().items.clear();
            push_notice(
                Notice::error("Could not install", TWO_SENTENCES),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let card = cx.debug_bounds("notices").expect("the card never drew");
        let message = cx
            .debug_bounds("notice-message")
            .expect("the message never drew");
        assert_eq!(
            card.size.width, narrow.size.width,
            "a long message widened the card"
        );
        assert!(
            message.right() <= card.right(),
            "the message ran past the right edge of the card: {message:?} in {card:?}"
        );
        assert!(
            message.size.height > one_line * 2.,
            "the message never wrapped: {} tall, one line is {one_line}",
            message.size.height
        );
        // Against the card's own corner, and to within a pixel. The card is
        // still animating in while this runs, so the two measurements are
        // taken a fraction of a frame apart and the absolute figures differ
        // by half a pixel about once in twenty runs. What the assertion is
        // for — the button being pushed around by the text it sits beside —
        // moves it much further than that.
        let moved = cx
            .debug_bounds("dismiss-notice")
            .expect("the dismiss button never drew");
        let before = (
            narrow.right() - dismiss.right(),
            dismiss.origin.y - narrow.origin.y,
        );
        let after = (card.right() - moved.right(), moved.origin.y - card.origin.y);
        assert!(
            (before.0 - after.0).abs() < px(1.) && (before.1 - after.1).abs() < px(1.),
            "the dismiss button moved as the message grew: {before:?} then {after:?}"
        );
    }

    /// The message a successful install leaves is long too, and it carries a
    /// button. The button stays under the wrapped sentence rather than being
    /// pushed off the side of the card.
    #[gpui_kit::test]
    fn a_long_message_wraps_above_its_button(cx: &mut TestAppContext) {
        let (mut cx, _) = window(cx);
        cx.update(|window, cx| {
            push_notice(
                Notice::info("Installed", AFTER_AN_INSTALL)
                    .action("Link to Claude Code", |_, _| {}),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let card = cx.debug_bounds("notices").expect("the card never drew");
        let message = cx
            .debug_bounds("notice-message")
            .expect("the message never drew");
        let action = cx
            .debug_bounds("notice-action")
            .expect("the action button never drew");
        assert!(
            message.size.height > px(30.),
            "the message never wrapped: {} tall",
            message.size.height
        );
        assert!(
            message.right() <= card.right() && action.right() <= card.right(),
            "the row ran past the right edge of the card: {message:?}, {action:?} in {card:?}"
        );
        assert!(
            action.top() >= message.bottom(),
            "the button landed beside the message rather than under it"
        );
    }

    /// Past the height cap the rows scroll inside the card, and the row the
    /// reader is waiting on is the newest one, at the bottom.
    #[gpui_kit::test]
    fn a_full_card_scrolls_and_shows_the_newest_row(cx: &mut TestAppContext) {
        let (mut cx, _) = window(cx);
        cx.update(|window, cx| {
            for index in 0..6 {
                push_notice(
                    Notice::error(format!("Could not install {index}"), TWO_SENTENCES),
                    window,
                    cx,
                );
            }
        });
        cx.run_until_parked();

        let card = cx.debug_bounds("notices").expect("the card never drew");
        assert_eq!(
            card.size.height,
            px(NOTICES_MAX_HEIGHT),
            "six wrapped notices grew the card past its cap"
        );
        cx.update(|_, cx| {
            let scroll = cx.default_global::<Notices>().scroll.clone();
            let max = scroll.max_offset().y;
            assert!(max > px(0.), "the rows did not overflow the card");
            assert_eq!(
                scroll.offset().y,
                -max,
                "the newest row was left below the fold"
            );
        });
    }

    /// Clearing the card has to forget the rows with it, or they would come
    /// back under the next notice pushed.
    #[gpui_kit::test]
    fn clearing_the_card_forgets_every_notice(cx: &mut TestAppContext) {
        let (mut cx, _) = window(cx);
        cx.update(|window, cx| {
            push_notice(Notice::error("One", "A"), window, cx);
            push_notice(Notice::error("Two", "B"), window, cx);
        });
        cx.run_until_parked();
        assert_eq!(headings(&mut cx).len(), 2);

        cx.update(|window, cx| {
            Root::update(window, cx, |root, window, cx| {
                root.clear_notifications(window, cx);
            });
        });
        // The card stays mounted until its exit transition is over, and it is
        // the unmount that clears the rows.
        cx.background_executor.advance_clock(Duration::from_secs(1));
        cx.run_until_parked();

        assert!(headings(&mut cx).is_empty(), "the rows outlived the card");
        assert_eq!(cards(&mut cx), 0);

        cx.update(|window, cx| push_notice(Notice::error("Three", "C"), window, cx));
        cx.run_until_parked();
        assert_eq!(headings(&mut cx), ["Three"]);
    }

    /// A delete that took something away offers to put it back. One that took
    /// nothing away has nothing to offer, and says so and goes.
    #[gpui_kit::test]
    fn only_a_delete_that_changed_something_offers_an_undo(cx: &mut TestAppContext) {
        let (mut cx, skillbase) = window(cx);
        let roots = cx.update(|_, cx| skillbase.read(cx).roots.clone());

        cx.update(|window, cx| {
            report_delete(
                "Deleted",
                "Could not delete",
                Ok(Outcome::one(Change::NoChange {
                    path: PathBuf::from("/nowhere"),
                    reason: "not there",
                })),
                &roots,
                Rc::new(|_, _| {}),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        assert!(
            headings(&mut cx).is_empty(),
            "a delete that did nothing left a message that stays"
        );

        cx.update(|window, cx| {
            report_delete(
                "Deleted",
                "Could not delete",
                Ok(Outcome::one(Change::MovedToTrash {
                    from: roots.store_dir().join("alpha"),
                    to: roots.trash_dir().join("alpha-1"),
                })),
                &roots,
                Rc::new(|_, _| {}),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(headings(&mut cx), ["Deleted"]);
        assert!(
            cx.debug_bounds("notices").is_some(),
            "the delete's message never drew"
        );
    }

    /// The two delete dialogs describe the same operation, so they say it in
    /// the same sentence.
    #[test]
    fn the_delete_sentence_counts_what_is_going() {
        let one = delete_effect(1);
        assert!(
            one.starts_with("The directory moves to ~/.skillbase/trash"),
            "{one}"
        );
        assert!(one.contains("its links are removed"), "{one}");
        assert!(one.contains("offers to put it back"), "{one}");

        let many = delete_effect(3);
        assert!(
            many.starts_with("The directories move to ~/.skillbase/trash"),
            "{many}"
        );
        assert!(many.contains("their links are removed"), "{many}");
        assert!(many.contains("offers to put them back"), "{many}");
    }
}
