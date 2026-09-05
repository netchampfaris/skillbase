//! The application's own asset source.
//!
//! `gpui-kit` ships a Lucide icon set and generates its `IconName` enum from
//! those 101 filenames, but the *bytes* behind each name are resolved at
//! runtime through whatever [`AssetSource`] the application registered. There
//! is exactly one source per application and no chaining, so overriding the
//! icon set means owning the source and delegating.
//!
//! `assets/icons` therefore holds a Phosphor duotone icon under every one of
//! those 101 Lucide filenames. The enum keeps Lucide's vocabulary — `Search`,
//! `TriangleAlert` — and the artwork is Phosphor throughout.
//!
//! Two things make duotone work here. GPUI rasterises an SVG to an *alpha
//! mask* and tints it with one colour, discarding every hue in the file; and
//! Phosphor's duotone weight is a single colour at two opacities. The
//! secondary path's `opacity="0.2"` survives the mask as 20% alpha, so the
//! icon reads as duotone in whatever the theme's foreground happens to be, in
//! light and dark alike.
//!
//! `assets/icons/agents` holds the brand marks the sidebar uses, which are not
//! part of the generated enum and are addressed by path.

use std::borrow::Cow;

use gpui_kit::{AssetSource, Result, SharedString};
use rust_embed::RustEmbed;

/// The files under `assets/`, embedded in release builds and read from disk in
/// debug builds, which is `rust-embed`'s default and means an icon can be
/// swapped without a rebuild.
#[derive(RustEmbed)]
#[folder = "assets"]
#[include = "icons/**/*.svg"]
struct Embedded;

/// The source registered with the application.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path.is_empty() {
            return Ok(None);
        }
        if let Some(file) = Embedded::get(path) {
            return Ok(Some(file.data));
        }
        // Every icon the framework names has a file of ours, so reaching this
        // is a missing asset rather than a normal miss. Fall back rather than
        // paint nothing: a Lucide glyph in the wrong family is a visible bug
        // report, and an invisible control is not.
        match gpui_kit::assets::Assets.load(path) {
            Ok(found) => Ok(found),
            // The bundled source returns an error for a miss rather than
            // `None`, and "neither source has it" is not worth failing over.
            Err(_) => Ok(None),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut listed: Vec<SharedString> = Embedded::iter()
            .filter(|item| item.starts_with(path))
            .map(|item| SharedString::from(item.to_string()))
            .collect();
        if let Ok(bundled) = gpui_kit::assets::Assets.list(path) {
            let extra: Vec<SharedString> = bundled
                .into_iter()
                .filter(|item| !listed.contains(item))
                .collect();
            listed.extend(extra);
        }
        Ok(listed)
    }
}
