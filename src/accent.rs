//! The operating system's accent colour.
//!
//! macOS draws the focus ring, the selected row of a list, and the on state of
//! a switch in the colour chosen under System Settings → Appearance, so an
//! application that picks its own hue looks like it belongs to a different
//! system. Skillbase reads the setting and follows it. Which roles take the
//! colour, and at what strength, is `theme::follow_accent`'s decision; this
//! module only reports what the setting says.
//!
//! `NSColor::controlAccentColor` is the whole answer. It is a dynamic catalog
//! colour: AppKit resolves the user's choice — including the default,
//! Multicolour, which resolves to the same blue every other application shows
//! — against the current light or dark appearance. Reading the underlying
//! `AppleAccentColor` preference would additionally distinguish Multicolour
//! from an explicitly chosen Blue, but nothing here wants to: the point is to
//! draw what the platform draws.
//!
//! Four of the eight hues differ between light and dark, so the appearance is
//! pinned rather than inherited from whatever `NSApp` currently believes.
//!
//! Everywhere that is not macOS has no equivalent setting, so [`current`]
//! returns `None` and the theme's own grey stands.
//!
//! The setting can change while the application is running, so [`observe`]
//! registers for the notification macOS posts when it does.

use gpui_kit::App;

/// The accent colour the system is set to, as sRGB components in `0.0..=1.0`.
///
/// `None` means "no system colour to follow" — the caller keeps whatever the
/// theme file specifies.
///
/// `dark` selects which appearance to resolve against.
pub fn current(dark: bool) -> Option<[f32; 4]> {
    imp::control_accent(dark)
}

/// Call `on_change` whenever the user picks a different accent colour.
///
/// The observer lives as long as the process. Nothing to observe off macOS,
/// where there is no such setting.
pub fn observe(cx: &mut App, on_change: fn(&mut App)) {
    imp::observe(cx, on_change);
}

#[cfg(target_os = "macos")]
mod imp {
    use std::cell::Cell;
    use std::ptr::NonNull;

    use block2::RcBlock;
    use gpui_kit::App;
    use objc2_app_kit::{
        NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSColor, NSColorSpace,
    };
    use objc2_foundation::{
        NSDistributedNotificationCenter, NSNotification, NSOperationQueue, NSString,
    };

    /// What macOS posts on the distributed notification centre when the accent
    /// or highlight colour changes. There is no constant for it in any
    /// framework header, so the string is the interface.
    const ACCENT_CHANGED: &str = "AppleColorPreferencesChangedNotification";

    pub(super) fn observe(cx: &mut App, on_change: fn(&mut App)) {
        let async_cx = cx.to_async();
        let executor = cx.foreground_executor().clone();
        let block = RcBlock::new(move |_: NonNull<NSNotification>| {
            let cx = async_cx.clone();
            // Hand the work to the next turn of the run loop rather than
            // doing it here. `AsyncApp::update` borrows the application, and
            // a notification is delivered by whatever run loop happens to be
            // running — including a nested one inside a GPUI callback that is
            // already holding that borrow.
            executor
                .spawn(async move {
                    let _ = cx.update(on_change);
                })
                .detach();
        });

        let name = NSString::from_str(ACCENT_CHANGED);
        let center = NSDistributedNotificationCenter::defaultCenter();
        // SAFETY: the name is a plain string, there is no filter object, and
        // the main queue keeps the block on the thread that owns the
        // `ForegroundExecutor` it captures.
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(&name),
                None,
                Some(&NSOperationQueue::mainQueue()),
                &block,
            )
        };
        // The centre holds the token, and the application never stops caring
        // about the accent, so there is nothing to unregister.
        std::mem::forget(token);
    }

    pub(super) fn control_accent(dark: bool) -> Option<[f32; 4]> {
        let name = if dark {
            unsafe { NSAppearanceNameDarkAqua }
        } else {
            unsafe { NSAppearanceNameAqua }
        };

        let srgb = NSColorSpace::sRGBColorSpace();
        let read = || {
            let accent = NSColor::controlAccentColor();
            // A catalog colour has no components until it is converted, and
            // the conversion is what can fail.
            let color = accent.colorUsingColorSpace(&srgb)?;
            Some([
                color.redComponent() as f32,
                color.greenComponent() as f32,
                color.blueComponent() as f32,
                color.alphaComponent() as f32,
            ])
        };

        let Some(appearance) = NSAppearance::appearanceNamed(name) else {
            // No such appearance: read against the current one, which is still
            // better than abandoning the accent altogether.
            return read();
        };

        let out = Cell::new(None);
        appearance.performAsCurrentDrawingAppearance(&RcBlock::new(|| out.set(read())));
        out.into_inner()
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use gpui_kit::App;

    pub(super) fn control_accent(_dark: bool) -> Option<[f32; 4]> {
        None
    }

    pub(super) fn observe(_cx: &mut App, _on_change: fn(&mut App)) {}
}
