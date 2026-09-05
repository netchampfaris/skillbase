//! The three panes and the view model they render.

pub mod detail;
pub mod list;
pub mod model;
pub mod settings;
pub mod sidebar;

use gpui_kit::component::notification::Notification;
use gpui_kit::component::{Icon, IconName, WindowExt as _};
use gpui_kit::{App, Window};
use skillbase_core::{AgentDef, InstallError, Outcome, Roots};

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
