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

use anyhow::{Context as _, Result};
use gpui_kit::App;
use gpui_kit::component::{Theme, ThemeRegistry};

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
    Ok(())
}
