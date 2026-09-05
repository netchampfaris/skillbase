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
//! `#181818` is `#2A2A2D`.
//!
//! The four roles that carry the interaction accent — `ring`, `selection`,
//! `drag_border` and `drop_target` — are grey in the file and overwritten at
//! runtime with the operating system's accent colour by [`follow_accent`]. The
//! file therefore holds the fallback, which is what shows on a system with no
//! such setting. macOS's own Graphite accent supplies the grey, so the
//! fallback is a colour the platform already uses rather than an invention.

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
    theme.colors.ring = color;
    theme.colors.selection = color;
    theme.colors.drag_border = color;
    theme.colors.drop_target = color.opacity(0.2);
    Theme::sync_base(cx);
}
