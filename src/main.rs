//! Skillbase — manage Agent Skills across every coding agent on this machine.

mod accent;
mod app;
mod assets;
mod menus;
mod theme;
mod ui;

use gpui_kit::component::{Root, Theme};
use gpui_kit::{
    App, AppContext as _, Bounds, DisplayId, Pixels, TitlebarOptions, Window, WindowBounds,
    WindowOptions, point, px, size,
};

use crate::app::Skillbase;
use crate::ui::model::{Preferences, WindowFrame, fit_to_display};

/// Roomy enough for three panes at their comfortable widths.
const DEFAULT_SIZE: (f32, f32) = (1320., 860.);
/// The narrowest window in which all three panes still do their job.
const MIN_SIZE: (f32, f32) = (860., 520.);

/// A window's bounds as the plain numbers the settings file keeps.
///
/// The conversion lives here rather than in `model` so that the fitting there
/// stays a pure function with no GPUI types in it, and so there is one place
/// that decides what "where the window was" means.
pub(crate) fn window_frame(bounds: Bounds<Pixels>) -> WindowFrame {
    WindowFrame {
        x: f32::from(bounds.origin.x),
        y: f32::from(bounds.origin.y),
        width: f32::from(bounds.size.width),
        height: f32::from(bounds.size.height),
    }
}

/// Which display a window is on, spelled the way the settings file keeps it.
///
/// The UUID rather than the display id: the id is a number the window server
/// hands out and reuses, so it names a different monitor after a reboot or
/// after one has been unplugged.
pub(crate) fn window_display(window: &Window, cx: &App) -> Option<String> {
    window.display(cx)?.uuid().ok().map(|id| id.to_string())
}

/// Where to open the window, and on which display: where it was last time, if
/// that is still a place the user can reach, and centred at the default size
/// otherwise.
///
/// The displays are read now rather than remembered, because the answer only
/// means anything against the desktop that exists at this moment. A saved frame
/// is measured from its display's own corner, so the display has to be settled
/// first — and it has to be named to the window as well, or the frame would be
/// applied to whichever display happens to be the primary one.
fn opening_placement(cx: &App) -> (WindowBounds, Option<DisplayId>) {
    let centred = || {
        (
            WindowBounds::centered(size(px(DEFAULT_SIZE.0), px(DEFAULT_SIZE.1)), cx),
            None,
        )
    };
    let Ok((roots, _)) = crate::ui::model::resolve_roots() else {
        return centred();
    };
    let preferences = Preferences::load(&roots).preferences;
    let Some(saved) = preferences.window else {
        return centred();
    };
    // The display it was on, while that display is still attached. Otherwise
    // the primary one, which is the display the user is certainly looking at:
    // the same numbers, fitted to the screen they can see.
    let Some(display) = preferences
        .display
        .and_then(|uuid| {
            cx.displays()
                .into_iter()
                .find(|display| display.uuid().is_ok_and(|id| id.to_string() == uuid))
        })
        .or_else(|| cx.primary_display())
    else {
        return centred();
    };

    // `visible_bounds` rather than `bounds`: it is the same measure from the
    // same corner, with the menu bar and the Dock taken out, so a window that
    // has to be moved does not land under either of them.
    match fit_to_display(saved, window_frame(display.visible_bounds()), MIN_SIZE) {
        Some(frame) => (
            WindowBounds::Windowed(Bounds {
                origin: point(px(frame.x), px(frame.y)),
                size: size(px(frame.width), px(frame.height)),
            }),
            Some(display.id()),
        ),
        None => centred(),
    }
}

/// What the window is called.
///
/// A run under `SKILLBASE_HOME` reads and writes a throwaway tree instead of
/// the real one, and the README points people at it for anything destructive.
/// The sidebar marks it too, but the sidebar collapses, so the title carries
/// it as well: it is the one part of the window that cannot be hidden.
fn window_title() -> String {
    match crate::ui::model::resolve_roots() {
        Ok((roots, true)) => format!(
            "Skillbase — {}={}",
            crate::ui::model::HOME_OVERRIDE_ENV,
            roots.home().display()
        ),
        _ => "Skillbase".to_string(),
    }
}

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

            menus::init(cx);
            // The skill list's own keys — move the selection, open what is
            // selected — are bound once here rather than in the view, because
            // `bind_keys` registers against the application and a view that is
            // rebuilt on every frame would register them again each time.
            ui::list::init(cx);

            // Screenshot mode. `script/preview.sh` needs the window rendered,
            // not in front: someone is usually working in another application
            // while it runs. `focus: false` makes AppKit order the window in
            // without making it key, and the activation below is skipped, so
            // nothing takes the keyboard.
            let quiet = std::env::var_os("SKILLBASE_NO_ACTIVATE").is_some();

            let (window_bounds, display_id) = opening_placement(cx);
            let options = WindowOptions {
                window_bounds: Some(window_bounds),
                display_id,
                window_min_size: Some(size(px(MIN_SIZE.0), px(MIN_SIZE.1))),
                titlebar: Some(TitlebarOptions {
                    title: None,
                    appears_transparent: true,
                    // 18pt from the container's AppKit origin, which is the
                    // bottom of a 48pt band when the lights are 12pt tall:
                    // padding above equals padding below, so they sit on the
                    // same centre line as the 24pt sidebar toggle.
                    traffic_light_position: Some(point(px(16.), px(18.))),
                }),
                // The header bands draw themselves and move the window with
                // `start_window_move`, so AppKit must not treat them as a
                // system window-move region as well: it would handle double
                // clicks a second time and delay every click while it waits to
                // see whether one is coming.
                app_owns_titlebar_drag: true,
                focus: !quiet,
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
                        window.set_window_title(&window_title());

                        if !quiet {
                            cx.activate(true);
                        }

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

                        // And when they pick a different accent colour, which
                        // is a separate setting and a separate notification.
                        accent::observe(cx, theme::follow_accent);
                    })
                    .expect("failed to configure the Skillbase window");
            })
            .detach();
        });
}
