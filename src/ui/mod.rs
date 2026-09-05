//! The three panes and the view model they render.

pub mod detail;
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
use skillbase_core::{AgentDef, InstallError, Outcome, Roots};

/// The height of the first header band in every column.
///
/// The three columns share it so their headers form one band across the top of
/// the window, and the macOS traffic lights are centred in it by
/// `traffic_light_position` in `main`.
pub(crate) const BAND_HEIGHT: Pixels = px(48.);

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
pub fn report(
    title: &str,
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
                Notification::error(error.to_string()).title(title.to_string()),
                cx,
            );
            false
        }
    }
}
