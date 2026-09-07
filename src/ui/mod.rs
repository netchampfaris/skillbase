//! The three panes and the view model they render.

pub mod detail;
pub mod discover;
pub mod list;
pub mod model;
pub mod settings;
pub mod sidebar;

use gpui_kit::component::notification::Notification;
use gpui_kit::component::{Icon, IconName, InteractiveElementExt as _, WindowExt as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Context, Div, InteractiveElement as _, IntoElement, MouseButton, Pixels, Render,
    SharedString, Stateful, Window, WindowControlArea, div, px,
};
use skillbase_core::{
    AgentDef, FetchError, GitHub, InstallError, InstallOptions, Installed, Installer, Outcome,
    RemoteCache, Roots, SkillLocation, UreqHttp, install_from_github,
};

use crate::ui::model::display_path;

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
/// not back up. Returns true when the filesystem changed.
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
            window.push_notification(
                // A success says what already happened, so losing it costs
                // nothing. A refusal is the only account of why the disk did
                // not change, and the five seconds a notification otherwise
                // gets is long enough to look away from. It stays until it is
                // dismissed.
                Notification::error(error.to_string())
                    .title(failed.to_string())
                    .autohide(false),
                cx,
            );
            false
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
    cache.write(roots);
    installed
}

/// Report what an install or an update did, and hand back the name it landed
/// under so the caller can scan again and select it.
///
/// A [`FetchError`] is the one thing on this path the user cannot see for
/// themselves: nothing appears in the list, and without a sentence there is
/// nothing to say why.
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
            Some(name)
        }
        Err(error) => {
            window.push_notification(
                // Nothing appeared in the list, so this sentence is the whole
                // account of what went wrong. It stays until it is dismissed.
                Notification::error(error.to_string())
                    .title(failed.to_string())
                    .autohide(false),
                cx,
            );
            None
        }
    }
}
