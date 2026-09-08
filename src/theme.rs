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
//! The roles that carry the interaction accent are grey in the file and
//! overwritten at runtime with the operating system's accent colour by
//! [`follow_accent`]: `ring` and `drag_border`, the selected row in a list and
//! a table, the filled state of a switch and a checkbox, and the two washes,
//! `selection` and `drop_target`. The file therefore holds the fallback, which
//! is what shows on a system with no such setting. macOS's own Graphite accent
//! supplies the grey, so the fallback is a colour the platform already uses
//! rather than an invention.
//!
//! [`follow_accent`] does not hand the raw accent to all of those. Three of
//! the eight macOS accents are too light to be seen as a shape against a white
//! window, and every one of them dulls the text that sits on top of a tint. The
//! two rules that decide what each role gets are documented on
//! [`follow_accent`] itself.

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
///
/// The accent reaches a role by one of two rules, because the two kinds of
/// role fail in different ways.
///
/// **Rule 1, a mark drawn in the accent.** The focus ring, a drag's target
/// edge, the filled track of a switch that is on, and the filled box of a
/// checked checkbox all have to be seen against the window. WCAG 2.1 1.4.11
/// asks 3:1 of a non-text indicator, and Yellow, Green and Orange all fall
/// short of that on a white window. Discarding those three would leave a user
/// on Yellow with no accent anywhere and nothing to explain it, so the accent
/// is instead darkened against a light window, or lightened against a dark
/// one, until it clears 3:1 — see [`fit_contrast`]. Hue and saturation are
/// untouched, so Yellow still reads as yellow. The same number covers the
/// marks drawn *inside* those fills: the switch thumb and the checkbox tick
/// are painted in `background` and `primary.foreground`, which this palette
/// sets to the same colour as the window, so a fill that clears 3:1 against
/// the window clears 3:1 against its own thumb.
///
/// **Rule 2, a tint behind text.** A selected row in the list or the table is a
/// wash under a name and a description. At full strength it would swamp them,
/// so it is the accent at low alpha, and the alpha comes down from the 20% the
/// component clamps to until the palette's faintest text on it — `muted`
/// foreground, which the row's description uses — still clears the 4.5:1 WCAG
/// asks of body text. No macOS accent drives it below 15%, and one that could
/// not clear 4.5:1 at any alpha down to the floor is refused the same way rule
/// 1 refuses a hue: the theme's own grey stands. See [`fit_tint`].
///
/// `selection` and `drop_target` follow rule 2 as well, at their own caps.
pub fn follow_accent(cx: &mut App) {
    let dark = Theme::global(cx).mode.is_dark();
    let Some([r, g, b, a]) = accent::current(dark) else {
        return;
    };
    let accent: Hsla = Rgba { r, g, b, a }.into();

    let theme = Theme::global_mut(cx);
    let background = theme.colors.background;
    let muted_foreground = theme.colors.muted_foreground;
    let foreground = theme.colors.foreground;

    // Rule 1. `None` means no lightness of this hue reaches 3:1 on this
    // window, which no macOS accent does against either of this palette's two
    // backgrounds. Should a future palette produce it, the theme's own grey
    // stands: a focus ring nobody can see costs a keyboard user their place,
    // and that is worse than a ring which does not match the system.
    if let Some(mark) = fit_contrast(accent, background, MIN_MARK_CONTRAST) {
        theme.colors.ring = mark;
        theme.colors.drag_border = mark;
        // `Switch` and `Checkbox` read their filled state from
        // `tokens.primary`, while the primary `Button` reads
        // `tokens.button_primary`, which the palette resolved separately and
        // which stays near-black. Only the small filled controls move, which
        // is where macOS puts the accent too.
        theme.tokens.primary = mark.into();
    }

    // Rule 2. `None` means no alpha down to the floor keeps the text on the
    // tint legible, and as in rule 1 the theme's own grey stands rather than a
    // tint that fails the ratio it was picked to meet.
    if let Some(row) = fit_tint(accent, background, muted_foreground, MAX_ROW_ALPHA) {
        theme.colors.list_active = row;
        theme.tokens.list_active = row.into();
        theme.colors.table_active = row;
        theme.tokens.table_active = row.into();
    }

    // The row's border stays the theme's grey. It is what distinguishes a
    // selected row the keyboard has left from one it is on, which the list
    // draws in `ring`; making both the accent would erase that difference.

    if let Some(selection) = fit_tint(accent, background, foreground, MAX_SELECTION_ALPHA) {
        theme.colors.selection = selection;
        theme.tokens.selection = selection.into();
    }

    if let Some(drop_target) = fit_tint(accent, background, foreground, MAX_ROW_ALPHA) {
        theme.colors.drop_target = drop_target;
        theme.tokens.drop_target = drop_target.into();
    }

    Theme::sync_base(cx);
    // `sync_base` only replaces the globals. Nothing observes them, so without
    // this an accent change picked up from the system leaves the ring, the
    // selected row and the switches painted in the old colour until some other
    // event draws a frame.
    cx.refresh_windows();
}

/// The least contrast a mark drawn in the accent may have against the surface
/// behind it.
///
/// WCAG 2.1 1.4.11 Non-text Contrast.
const MIN_MARK_CONTRAST: f32 = 3.0;

/// The least contrast text on an accent tint may have against that tint.
///
/// WCAG 2.1 1.4.3 Contrast (Minimum), for text below 18pt.
const MIN_TEXT_CONTRAST: f32 = 4.5;

/// The strongest a selected row's tint may be.
///
/// `Theme::apply_config` clamps `list.active.background` to this, so anything
/// above it would be a value the palette itself could not express.
const MAX_ROW_ALPHA: f32 = 0.2;

/// The strongest a text selection's tint may be, which the same clamp puts
/// higher than a row's.
const MAX_SELECTION_ALPHA: f32 = 0.3;

/// Move `color` along its own lightness until it reaches `min` contrast
/// against `background`, keeping its hue and saturation.
///
/// Against a light surface that means darkening, against a dark one
/// lightening. Returns `color` unchanged when it already passes, and `None`
/// when even black or white in this hue would not reach `min`, which leaves
/// the caller to keep whatever it had.
fn fit_contrast(color: Hsla, background: Hsla, min: f32) -> Option<Hsla> {
    if contrast_ratio(color, background) >= min {
        return Some(color);
    }

    // Which way there is room to go. The accent is the lighter of the two when
    // it is failing against a dark window, so it has to get lighter still.
    let darken = relative_luminance(background) > relative_luminance(color);
    // One step is under half a percent of lightness, fine enough that the
    // result never overshoots into a visibly different colour.
    const STEPS: u32 = 256;
    for step in 1..=STEPS {
        let t = step as f32 / STEPS as f32;
        let l = if darken {
            color.l * (1.0 - t)
        } else {
            color.l + (1.0 - color.l) * t
        };
        let candidate = Hsla { l, ..color };
        if contrast_ratio(candidate, background) >= min {
            return Some(candidate);
        }
    }
    None
}

/// `color` at the strongest alpha up to `max` that leaves `text` legible on
/// the tint it composites to over `background`.
///
/// Steps down in the same increments the palette is written in — one percent —
/// and stops at a floor, because a tint too faint to see is no more use than
/// one that swallows the text. Returns `None` when even the floor does not
/// reach [`MIN_TEXT_CONTRAST`], which leaves the caller to keep whatever it
/// had, the same way [`fit_contrast`] does. No macOS accent reaches the floor
/// against either of this palette's backgrounds.
fn fit_tint(color: Hsla, background: Hsla, text: Hsla, max: f32) -> Option<Hsla> {
    /// The faintest a tint may become before legibility stops being the
    /// binding constraint.
    const MIN_ALPHA: f32 = 0.1;
    /// One percent, the increment the palette itself is written in.
    const STEP: f32 = 0.01;

    // Counted rather than accumulated, so twenty subtractions of 0.01 cannot
    // drift the floor comparison either way.
    let steps = (((max - MIN_ALPHA) / STEP).round() as i32).max(0);
    for step in 0..=steps {
        let alpha = max - step as f32 * STEP;
        let tint = background.blend(color.alpha(alpha));
        if contrast_ratio(text, tint) >= MIN_TEXT_CONTRAST {
            return Some(color.alpha(alpha));
        }
    }
    None
}

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
    /// The light palette's `background`, `foreground` and `muted.foreground`.
    const LIGHT_SURFACE: [(u8, u8, u8); 3] =
        [(0xFF, 0xFF, 0xFF), (0x17, 0x1A, 0x1D), (0x66, 0x66, 0x68)];
    /// The dark palette's, in the same order.
    const DARK_SURFACE: [(u8, u8, u8); 3] =
        [(0x18, 0x18, 0x18), (0xED, 0xED, 0xED), (0x9E, 0x9E, 0x9E)];

    /// The eight macOS accents as AppKit resolves them in the light
    /// appearance: Blue, Purple, Pink, Red, Orange, Yellow, Green, Graphite.
    const LIGHT_ACCENTS: [(u8, u8, u8); 8] = [
        (0x00, 0x7A, 0xFF),
        (0xA5, 0x50, 0xA7),
        (0xF7, 0x4F, 0x9E),
        (0xFF, 0x52, 0x57),
        (0xF7, 0x82, 0x1B),
        (0xFF, 0xC6, 0x00),
        (0x62, 0xBA, 0x46),
        (0x8C, 0x8C, 0x8C),
    ];

    /// The same eight in the dark appearance.
    const DARK_ACCENTS: [(u8, u8, u8); 8] = [
        (0x0A, 0x84, 0xFF),
        (0xBF, 0x5A, 0xF2),
        (0xFF, 0x37, 0x5F),
        (0xFF, 0x45, 0x3A),
        (0xFF, 0x9F, 0x0A),
        (0xFF, 0xD6, 0x0A),
        (0x32, 0xD7, 0x4B),
        (0x98, 0x98, 0x9D),
    ];

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
    fn the_faint_macos_accents_fail_the_mark_floor_on_a_white_window() {
        let white = hex(WHITE.0, WHITE.1, WHITE.2);
        // Orange, Yellow and Green, the three that made the old all-or-nothing
        // gate discard the accent altogether.
        for (r, g, b) in [LIGHT_ACCENTS[4], LIGHT_ACCENTS[5], LIGHT_ACCENTS[6]] {
            let ratio = contrast_ratio(hex(r, g, b), white);
            assert!(ratio < MIN_MARK_CONTRAST, "{r:02X}{g:02X}{b:02X} {ratio}");
        }
    }

    #[test]
    fn the_default_blue_accent_clears_the_mark_floor_on_a_white_window() {
        let ratio = contrast_ratio(hex(0x00, 0x7A, 0xFF), hex(WHITE.0, WHITE.1, WHITE.2));
        assert!(ratio >= MIN_MARK_CONTRAST, "{ratio}");
    }

    #[test]
    fn every_macos_accent_reaches_the_mark_floor_after_fitting() {
        for (accents, surface) in [(LIGHT_ACCENTS, LIGHT_SURFACE), (DARK_ACCENTS, DARK_SURFACE)] {
            let background = hex(surface[0].0, surface[0].1, surface[0].2);
            for (r, g, b) in accents {
                let fitted = fit_contrast(hex(r, g, b), background, MIN_MARK_CONTRAST)
                    .unwrap_or_else(|| panic!("{r:02X}{g:02X}{b:02X} could not be fitted"));
                let ratio = contrast_ratio(fitted, background);
                assert!(ratio >= MIN_MARK_CONTRAST, "{r:02X}{g:02X}{b:02X} {ratio}");
            }
        }
    }

    #[test]
    fn fitting_keeps_the_hue_so_yellow_still_reads_as_yellow() {
        let white = hex(WHITE.0, WHITE.1, WHITE.2);
        let yellow = hex(0xFF, 0xC6, 0x00);
        let fitted = fit_contrast(yellow, white, MIN_MARK_CONTRAST).expect("yellow fits");
        assert!(
            (fitted.h - yellow.h).abs() < 0.001,
            "{} {}",
            fitted.h,
            yellow.h
        );
        assert!(
            (fitted.s - yellow.s).abs() < 0.001,
            "{} {}",
            fitted.s,
            yellow.s
        );
        // Against a white window it can only have got darker.
        assert!(fitted.l < yellow.l, "{} {}", fitted.l, yellow.l);
    }

    #[test]
    fn an_accent_that_already_passes_is_left_alone() {
        let white = hex(WHITE.0, WHITE.1, WHITE.2);
        let blue = hex(0x00, 0x7A, 0xFF);
        assert_eq!(fit_contrast(blue, white, MIN_MARK_CONTRAST), Some(blue));
    }

    #[test]
    fn a_hue_that_cannot_reach_the_floor_is_refused_rather_than_approximated() {
        // Mid grey against mid grey: no lightness of a colourless hue clears
        // 21:1, so there is nothing to adopt and the caller keeps its own.
        let grey = hex(0x80, 0x80, 0x80);
        assert_eq!(fit_contrast(grey, grey, 21.0), None);
    }

    #[test]
    fn a_selected_row_keeps_its_description_legible_in_both_appearances() {
        for (accents, surface) in [(LIGHT_ACCENTS, LIGHT_SURFACE), (DARK_ACCENTS, DARK_SURFACE)] {
            let background = hex(surface[0].0, surface[0].1, surface[0].2);
            let muted = hex(surface[2].0, surface[2].1, surface[2].2);
            for (r, g, b) in accents {
                let row = fit_tint(hex(r, g, b), background, muted, MAX_ROW_ALPHA)
                    .unwrap_or_else(|| panic!("{r:02X}{g:02X}{b:02X} could not be fitted"));
                let tint = background.blend(row);
                let ratio = contrast_ratio(muted, tint);
                assert!(ratio >= MIN_TEXT_CONTRAST, "{r:02X}{g:02X}{b:02X} {ratio}");
                assert!(row.a <= MAX_ROW_ALPHA, "{r:02X}{g:02X}{b:02X} {}", row.a);
                // No accent costs more than a quarter of the alpha, so the
                // selection never fades to something a reader would miss.
                assert!(row.a >= 0.149, "{r:02X}{g:02X}{b:02X} {}", row.a);
            }
        }
    }

    #[test]
    fn a_tint_that_needs_no_help_keeps_the_full_alpha() {
        let white = hex(WHITE.0, WHITE.1, WHITE.2);
        let muted = hex(0x66, 0x66, 0x68);
        let yellow = hex(0xFF, 0xC6, 0x00);
        let row = fit_tint(yellow, white, muted, MAX_ROW_ALPHA).expect("yellow fits");
        assert!((row.a - MAX_ROW_ALPHA).abs() < 0.001, "{}", row.a);
    }

    #[test]
    fn a_tint_that_cannot_keep_text_legible_is_refused_rather_than_returned() {
        // A palette whose faintest text is the window colour itself: no alpha
        // of any accent makes white text on a near-white tint reach 4.5:1, so
        // there is nothing to adopt and the caller keeps its own.
        let white = hex(WHITE.0, WHITE.1, WHITE.2);
        let yellow = hex(0xFF, 0xC6, 0x00);
        assert_eq!(fit_tint(yellow, white, white, MAX_ROW_ALPHA), None);
    }
}
