//! Skillbase — manage Agent Skills across every coding agent on this machine.

mod accent;
mod app;
mod assets;
mod theme;
mod ui;

use gpui_kit::component::{Root, Theme};
use gpui_kit::{
    App, AppContext as _, TitlebarOptions, WindowBounds, WindowOptions, point, px, size,
};

use crate::app::Skillbase;

/// Roomy enough for three panes at their comfortable widths.
const DEFAULT_SIZE: (f32, f32) = (1320., 860.);
/// The narrowest window in which all three panes still do their job.
const MIN_SIZE: (f32, f32) = (860., 520.);

fn main() {
    gpui_kit::application()
        .with_assets(crate::assets::Assets)
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
                titlebar: Some(TitlebarOptions {
                    title: None,
                    appears_transparent: true,
                    // 18 = (48 - 12) / 2: a 12pt control centred in the 48pt
                    // header band the leftmost column opens with, so the lights
                    // sit on that band's centre line rather than above it.
                    traffic_light_position: Some(point(px(16.), px(18.))),
                }),
                // The header bands draw themselves and move the window with
                // `start_window_move`, so AppKit must not treat them as a
                // system window-move region as well: it would handle double
                // clicks a second time and delay every click while it waits to
                // see whether one is coming.
                app_owns_titlebar_drag: true,
                ..Default::default()
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
                                // Re-applying the theme config restores the
                                // grey fallback, so the accent has to be put
                                // back on top of it.
                                theme::follow_accent(cx);
                            })
                            .detach();
                    })
                    .expect("failed to configure the Skillbase window");
            })
            .detach();
        });
}
