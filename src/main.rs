//! Skillbase — manage Agent Skills across every coding agent on this machine.

mod app;
mod theme;
mod ui;

use gpui_kit::component::{Root, Theme, TitleBar};
use gpui_kit::{App, AppContext as _, WindowBounds, WindowOptions, px, size};

use crate::app::Skillbase;

/// Roomy enough for three panes at their comfortable widths.
const DEFAULT_SIZE: (f32, f32) = (1100., 720.);
/// The narrowest window in which all three panes still do their job.
const MIN_SIZE: (f32, f32) = (860., 520.);

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx: &mut App| {
            // Must come before anything that touches a component or a theme.
            gpui_kit::init(cx);

            if let Err(error) = theme::init(cx) {
                // The default palette still renders a usable window, so this
                // is worth reporting but not worth refusing to start over.
                eprintln!("skillbase: {error:#}");
            }

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(
                    size(px(DEFAULT_SIZE.0), px(DEFAULT_SIZE.1)),
                    cx,
                )),
                window_min_size: Some(size(px(MIN_SIZE.0), px(MIN_SIZE.1))),
                // Insets the macOS traffic lights over the title bar, which
                // carries the sidebar's colour, so the sidebar runs up under
                // them with no seam.
                ..TitleBar::window_options()
            };

            cx.spawn(async move |cx| {
                let window = cx
                    .open_window(options, |window, cx| {
                        let view = cx.new(|cx| Skillbase::new(window, cx));
                        cx.new(|cx| Root::new(view, window, cx))
                    })
                    .expect("failed to open the Skillbase window");

                window
                    .update(cx, |_, window, cx| {
                        window.set_window_title("Skillbase");
                        cx.activate(true);

                        // Follow the operating system when the user switches
                        // between light and dark.
                        window
                            .observe_window_appearance(|window, cx| {
                                Theme::sync_system_appearance(Some(window), cx);
                            })
                            .detach();
                    })
                    .expect("failed to configure the Skillbase window");
            })
            .detach();
        });
}
