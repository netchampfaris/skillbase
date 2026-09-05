//! The operating system's accent colour, for the focus ring.
//!
//! macOS draws every focus ring in the colour chosen under System Settings →
//! Appearance, so an application that picks its own hue looks like it belongs
//! to a different system. Skillbase reads the setting and follows it.
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
//! Everywhere that is not macOS has no equivalent setting, so [`focus_ring`]
//! returns `None` and the theme's own grey stands.

/// The accent colour to draw focus rings in, as sRGB components in `0.0..=1.0`.
///
/// `None` means "no system colour to follow" — the caller keeps whatever the
/// theme file specifies.
///
/// `dark` selects which appearance to resolve against.
pub fn focus_ring(dark: bool) -> Option<[f32; 4]> {
    imp::control_accent(dark)
}

#[cfg(target_os = "macos")]
mod imp {
    use std::cell::Cell;

    use block2::RcBlock;
    use objc2_app_kit::{
        NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSColor, NSColorSpace,
    };

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
    pub(super) fn control_accent(_dark: bool) -> Option<[f32; 4]> {
        None
    }
}
