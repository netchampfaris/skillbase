//! The Codex-derived palette, expressed as a `gpui-kit` theme set.
//!
//! Every colour in the application resolves from `cx.theme()` by semantic
//! role. The hex values live only in `assets/themes/skillbase.json`, which
//! carries a light and a dark variant of the same set of roles.
//!
//! Two entries in that file are compensated rather than literal, and the
//! reason belongs here rather than in JSON, which cannot hold a comment:
//! `Theme::apply_config` clamps `list.active.background` (and the table and
//! selection equivalents) to at most 20% alpha, so a selected row is always a
//! translucent tint over whatever sits behind it. The palette specifies the
//! *resolved* colour, so the file stores the tint that composites to it —
//! `#7D7D7D` at 20% over `#FFFFFF` is `#E5E5E5`, and `#A8A8B4` at 20% over
//! `#181818` is `#353537`.
//!
//! The four roles that carry the interaction accent — `ring`, `selection`,
//! `drag_border` and `drop_target` — are grey in the file and overwritten at
//! runtime with the operating system's accent colour by [`follow_accent`]. The
//! file therefore holds the fallback, which is what shows on a system with no
//! such setting. macOS's own Graphite accent supplies the grey, so the
//! fallback is a colour the platform already uses rather than an invention.
//! `ring` additionally keeps the file's value whenever the system accent is
//! too faint to see against the window — see [`follow_accent`].

use anyhow::{Context as _, Result};
use gpui_kit::App;
use gpui_kit::component::{Theme, ThemeRegistry};
use gpui_kit::{Hsla, Rgba};

use crate::accent;

/// The theme set, compiled in so the application has its palette with no
/// filesystem read at startup.
const THEME_SET: &str = include_str!("../assets/themes/skillbase.json");

const LIGHT: &str = "Skillbase Light";
const DARK: &str = "Skillbase Dark";

/// Register the Skillbase palette and follow the operating system appearance.
///
/// Call once, after `gpui_kit::init`.
pub fn init(cx: &mut App) -> Result<()> {
    ThemeRegistry::global_mut(cx)
        .load_themes_from_str(THEME_SET)
        .context("failed to parse the Skillbase theme set")?;

    let registry = ThemeRegistry::global(cx);
    let light = registry
        .themes()
        .get(LIGHT)
        .cloned()
        .with_context(|| format!("theme set has no `{LIGHT}` variant"))?;
    let dark = registry
        .themes()
        .get(DARK)
        .cloned()
        .with_context(|| format!("theme set has no `{DARK}` variant"))?;

    let theme = Theme::global_mut(cx);
    theme.light_theme = light;
    theme.dark_theme = dark;

    Theme::sync_system_appearance(None, cx);
    follow_accent(cx);
    Ok(())
}

/// Point the interaction accent at the operating system's accent colour.
///
/// Must run after every `Theme::sync_system_appearance`, not only at startup:
/// switching between light and dark re-applies the stored `ThemeConfig`, which
/// restores the grey from the file. There is nothing to undo when the system
/// has no accent to report — the grey is then the intended colour.
pub fn follow_accent(cx: &mut App) {
    let dark = Theme::global(cx).mode.is_dark();
    let Some([r, g, b, a]) = accent::focus_ring(dark) else {
        return;
    };
    let color: Hsla = Rgba { r, g, b, a }.into();

    let theme = Theme::global_mut(cx);
    // `ring` is the focus ring proper. The other three are the same accent
    // doing the same job elsewhere — a drag's target edge, a drop target's
    // wash, and selected text — and leaving them grey while the ring turns
    // blue would read as two unrelated decisions.
    //
    // The ring is the one of the four that has to be legible on its own: it is
    // the only marker of which control the keyboard is on, so if it disappears
    // the keyboard user loses their place. Several macOS accents are too light
    // to carry that against a white window — Yellow, Green and Orange all fall
    // short — so the accent is adopted for `ring` only when it clears the 3:1
    // that WCAG asks of a non-text indicator, and the theme's own grey stands
    // when it does not. The other three are decoration over a legible
    // foreground, so they take the accent either way.
    if contrast_ratio(color, theme.colors.background) >= MIN_RING_CONTRAST {
        theme.colors.ring = color;
    }
    theme.colors.selection = color.opacity(0.3);
    theme.colors.drag_border = color;
    theme.colors.drop_target = color.opacity(0.2);
    Theme::sync_base(cx);
}

/// The least contrast a focus ring may have against the surface behind it.
///
/// WCAG 2.1 1.4.11 Non-text Contrast.
const MIN_RING_CONTRAST: f32 = 3.0;

/// The WCAG contrast ratio between two colours, from 1.0 to 21.0.
///
/// `foreground` is composited over `background` first, so a translucent accent
/// is measured as it will actually be seen. Neither `gpui` nor `gpui-kit`
/// exposes this, hence the local copy.
fn contrast_ratio(foreground: Hsla, background: Hsla) -> f32 {
    let front = relative_luminance(background.blend(foreground));
    let back = relative_luminance(background);
    let (lighter, darker) = if front >= back {
        (front, back)
    } else {
        (back, front)
    };
    (lighter + 0.05) / (darker + 0.05)
}

/// Relative luminance per WCAG 2.1, from an opaque sRGB colour.
fn relative_luminance(color: Hsla) -> f32 {
    let rgb = color.to_rgb();
    let channel = |c: f32| {
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(rgb.r) + 0.7152 * channel(rgb.g) + 0.0722 * channel(rgb.b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::Rgba;

    fn hex(r: u8, g: u8, b: u8) -> Hsla {
        Rgba {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a: 1.0,
        }
        .into()
    }

    const WHITE: (u8, u8, u8) = (0xFF, 0xFF, 0xFF);

    #[test]
    fn contrast_of_black_on_white_is_the_maximum() {
        let ratio = contrast_ratio(hex(0, 0, 0), hex(WHITE.0, WHITE.1, WHITE.2));
        assert!((ratio - 21.0).abs() < 0.01, "{ratio}");
    }

    #[test]
    fn contrast_is_symmetric() {
        let a = hex(0x76, 0x76, 0x76);
        let b = hex(WHITE.0, WHITE.1, WHITE.2);
        assert!((contrast_ratio(a, b) - contrast_ratio(b, a)).abs() < 0.001);
    }

    #[test]
    fn the_faint_macos_accents_fail_the_ring_floor_on_a_white_window() {
        let white = hex(WHITE.0, WHITE.1, WHITE.2);
        // Yellow, Green and Orange as macOS resolves them in the light
        // appearance.
        for (r, g, b) in [(0xFF, 0xC6, 0x00), (0x62, 0xBA, 0x46), (0xF7, 0x82, 0x1B)] {
            let ratio = contrast_ratio(hex(r, g, b), white);
            assert!(ratio < MIN_RING_CONTRAST, "{r:02X}{g:02X}{b:02X} {ratio}");
        }
    }

    #[test]
    fn the_default_blue_accent_clears_the_ring_floor_on_a_white_window() {
        let ratio = contrast_ratio(hex(0x00, 0x7A, 0xFF), hex(WHITE.0, WHITE.1, WHITE.2));
        assert!(ratio >= MIN_RING_CONTRAST, "{ratio}");
    }
}
