//! The three panes and the view model they render.

pub mod detail;
pub mod list;
pub mod model;
pub mod settings;
pub mod sidebar;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::{App, Window};
use skillbase_core::{InstallError, Outcome, Roots};

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
