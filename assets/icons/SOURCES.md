# Icon sources

This directory is a wholesale replacement for the Lucide icon set bundled with
`gpui-kit-assets`. The app serves it through its own `AssetSource`, so every file
name here must match a file name in
`gpui-kit-assets-0.6.0/assets/icons/` exactly. There are 101 of them.

## Upstream

- **Phosphor Icons**, `@phosphor-icons/core` version **2.1.1**, *duotone* weight
  (one file, `star-fill.svg`, uses the *fill* weight instead).
- Fetched from `https://cdn.jsdelivr.net/npm/@phosphor-icons/core@2.1.1/assets/duotone/<name>-duotone.svg`.
- Licence: **MIT** — <https://github.com/phosphor-icons/core/blob/main/LICENSE>
- Project: <https://phosphoricons.com>

Phosphor artwork is copied verbatim: `viewBox="0 0 256 256"`, `fill="currentColor"`,
one path at `opacity="0.2"` and one solid path. No `width`/`height` attributes, no
hex colours, no `<style>`, no `url(...)`.

## Name mapping

`gpui-kit` names are Lucide names; the file name is what `icons/<name>.svg`
resolves to at runtime.

| gpui-kit / Lucide name | Phosphor icon |
| --- | --- |
| `a-large-small` | `text-aa` |
| `arrow-down` | `arrow-down` |
| `arrow-left` | `arrow-left` |
| `arrow-right` | `arrow-right` |
| `arrow-up` | `arrow-up` |
| `asterisk` | `asterisk` |
| `battery` | `battery-empty` |
| `battery-charging` | `battery-charging` |
| `battery-full` | `battery-full` |
| `battery-low` | `battery-low` |
| `battery-medium` | `battery-medium` |
| `battery-warning` | `battery-warning` |
| `bell` | `bell` |
| `book-open` | `book-open` |
| `bot` | `robot` |
| `building-2` | `buildings` |
| `calendar` | `calendar-blank` |
| `case-sensitive` | `text-aa` |
| `chart-pie` | `chart-pie-slice` |
| `check` | `check` |
| `chevron-down` | `caret-down` |
| `chevron-left` | `caret-left` |
| `chevron-right` | `caret-right` |
| `chevron-up` | `caret-up` |
| `chevrons-up-down` | `caret-up-down` |
| `circle-check` | `check-circle` |
| `circle-user` | `user-circle` |
| `circle-x` | `x-circle` |
| `close` | `x` |
| `copy` | `copy` |
| `cpu` | `cpu` |
| `dash` | `minus` |
| `delete` | `trash` |
| `ellipsis` | `dots-three` |
| `ellipsis-vertical` | `dots-three-vertical` |
| `external-link` | `arrow-square-out` |
| `eye` | `eye` |
| `eye-off` | `eye-slash` |
| `file` | `file` |
| `file-text` | `file-text` |
| `folder` | `folder` |
| `folder-closed` | `folder` |
| `folder-open` | `folder-open` |
| `frame` | `frame-corners` |
| `gallery-vertical-end` | `stack` |
| `github` | `github-logo` |
| `globe` | `globe` |
| `hard-drive` | `hard-drive` |
| `heart` | `heart` |
| `heart-off` | `heart-break` |
| `inbox` | `tray` |
| `info` | `info` |
| `layout-dashboard` | `squares-four` |
| `loader` | `spinner-gap` |
| `loader-circle` | `circle-notch` |
| `map` | `map-trifold` |
| `maximize` | `corners-out` |
| `memory-stick` | `memory` |
| `menu` | `list` |
| `minimize` | `corners-in` |
| `minus` | `minus` |
| `moon` | `moon` |
| `network` | `network` |
| `palette` | `palette` |
| `panel-left` | `sidebar-simple` |
| `pause` | `pause` |
| `play` | `play` |
| `plus` | `plus` |
| `redo` | `arrow-arc-right` |
| `redo-2` | `arrow-u-up-right` |
| `replace` | `swap` |
| `rotate-cw` | `arrow-clockwise` |
| `search` | `magnifying-glass` |
| `settings` | `gear` |
| `settings-2` | `sliders-horizontal` |
| `sort-ascending` | `sort-ascending` |
| `sort-descending` | `sort-descending` |
| `square-terminal` | `terminal-window` |
| `star` | `star` |
| `star-fill` | `star (fill weight)` |
| `sun` | `sun` |
| `thumbs-down` | `thumbs-down` |
| `thumbs-up` | `thumbs-up` |
| `triangle-alert` | `warning` |
| `undo` | `arrow-arc-left` |
| `undo-2` | `arrow-u-up-left` |
| `user` | `user` |
| `window-close` | `x` |
| `window-maximize` | `square` |
| `window-minimize` | `minus` |
| `window-restore` | `copy` |
| `inspector` | *hand-authored* |
| `panel-bottom` | *hand-authored* |
| `panel-bottom-open` | *hand-authored* |
| `panel-left-close` | *hand-authored* |
| `panel-left-open` | *hand-authored* |
| `panel-right` | *hand-authored* |
| `panel-right-close` | *hand-authored* |
| `panel-right-open` | *hand-authored* |
| `resize-corner` | *hand-authored* |
| `star-off` | *hand-authored* |

### Deliberate deviations from a literal Lucide match

- `delete` — Lucide's `delete` glyph is a backspace key. Phosphor `trash` is used
  instead, because `IconName::Delete` is what application code reaches for when it
  wants a destructive-delete affordance and this set has no other trash glyph.
- `a-large-small` and `case-sensitive` both map to `text-aa`; Phosphor has no
  second "Aa" variant that keeps the meaning.
- `folder` and `folder-closed` both map to `folder`.
- `close` and `window-close` both map to `x`; `dash` and `minus` both map to `minus`.
- `sort-ascending` / `sort-descending` use Phosphor's own glyphs, which show the
  arrow pointing the opposite way to the Lucide chevrons they replace. This is
  Phosphor's convention for those words and it is kept.

## Hand-authored files

Ten files have no Phosphor equivalent. All are drawn in Phosphor's geometry:
`viewBox="0 0 256 256"`, a 16-unit line weight, 8-unit corner radii on the
duotone layer, `currentColor` throughout. Carets and free strokes are drawn as
`stroke-width="16"` round-capped paths so their weight matches the filled Phosphor
outlines exactly.

- **`inspector.svg`** — Phosphor `selection` (dashed frame) with the `cursor` fill-weight pointer scaled to 46% and centred inside it.
- **`panel-bottom.svg`** — `sidebar-simple` window redrawn with a horizontal divider at y=160 and the bottom band filled at 0.2.
- **`panel-bottom-open.svg`** — panel-bottom plus an upward caret in the main region.
- **`panel-left-close.svg`** — Phosphor `sidebar-simple` plus a left-pointing caret in the main region.
- **`panel-left-open.svg`** — Phosphor `sidebar-simple` plus a right-pointing caret in the main region.
- **`panel-right.svg`** — Phosphor `sidebar-simple` mirrored about x=128 (coordinates rewritten, arc sweep flags flipped).
- **`panel-right-close.svg`** — Mirrored `sidebar-simple` plus a right-pointing caret in the main region.
- **`panel-right-open.svg`** — Mirrored `sidebar-simple` plus a left-pointing caret in the main region.
- **`resize-corner.svg`** — Three parallel diagonal strokes in the bottom-right corner, 16-unit round-capped, no 0.2 layer.
- **`star-off.svg`** — Phosphor `star` duotone with a 16-unit slash from (48,40) to (208,216), matching the angle of Phosphor `eye-slash`.

The eight `panel-*` files are one visually consistent family: the same rounded
window outline from Phosphor `sidebar-simple`, the panel edge filled at
`opacity="0.2"`, and a caret on the `-open` / `-close` variants pointing the way
the panel will move.

`window-close`, `window-minimize`, `window-maximize` and `window-restore` are not
hand-drawn: real Phosphor icons (`x`, `minus`, `square`, `copy`) are already the
correct title-bar glyphs. They are never rendered on macOS, which uses native
traffic lights, but the files must exist so no icon path 404s.
