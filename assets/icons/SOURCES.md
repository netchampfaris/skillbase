# Icon sources

This directory is a wholesale replacement for the Lucide icon set bundled with
`gpui-kit-assets`. The app serves it through its own `AssetSource`, so every file
name here must match a file name in
`gpui-kit-assets-0.6.0/assets/icons/` exactly. There are 101 of them.

## Upstream

- **HugeIcons**, `@hugeicons/core-free-icons` version **4.3.0**, the
  *stroke-rounded* variant, which is the package's default export.
- Licence: **MIT** — <https://github.com/hugeicons/hugeicons-react/blob/main/LICENSE>
- Project: <https://hugeicons.com>

The package ships icons as ES modules — arrays of `[tag, attributes]` pairs — not
as SVG files, so each file here was generated from its module: the attributes are
rewritten from React's camelCase to SVG's kebab-case and wrapped in
`<svg viewBox="0 0 24 24" fill="none">`. The geometry is copied verbatim.
Every path is `stroke="currentColor"` at `stroke-width="1.5"` with round caps and
joins. No `width`/`height` attributes, no hex colours, no `<style>`, no `url(...)`.

One file deviates: `star-fill.svg` is `Star` with `fill="currentColor"` added,
because the free set has no solid star and the slot exists to sit beside `star`
as its filled counterpart.

Every one of the 101 names has a real HugeIcons glyph. Nothing here is
hand-authored, which was not true of the Phosphor set this replaces.

## Name mapping

`gpui-kit` names are Lucide names; the file name is what `icons/<name>.svg`
resolves to at runtime. HugeIcons names are the module's, minus the `Icon`
suffix.

| gpui-kit / Lucide name | HugeIcons icon | Note |
| --- | --- | --- |
| `a-large-small` | `ALargeSmall` | |
| `arrow-down` | `ArrowDown02` | `…01` is a chevron; the arrows are the `…02` family |
| `arrow-left` | `ArrowLeft02` | |
| `arrow-right` | `ArrowRight02` | |
| `arrow-up` | `ArrowUp02` | |
| `asterisk` | `Asterisk02` | bare `Asterisk` wraps it in a badge |
| `battery` | `BatteryEmpty` | shares its body and terminal with the other five |
| `battery-charging` | `BatteryCharging01` | |
| `battery-full` | `BatteryFull` | |
| `battery-low` | `BatteryLow` | |
| `battery-medium` | `BatteryMedium01` | two bars, between Low's one and Full's three |
| `battery-warning` | `BatteryWarning` | |
| `bell` | `Bell` | |
| `book-open` | `BookOpen01` | |
| `bot` | `Bot` | |
| `building-2` | `Building` | a main block with a lower annex, so it reads as plural |
| `calendar` | `Calendar04` | the only variant with a blank interior |
| `case-sensitive` | `CaseSensitive` | |
| `chart-pie` | `PieChart` | |
| `check` | `Check` | |
| `chevron-down` | `ChevronDown` | |
| `chevron-left` | `ChevronLeft` | |
| `chevron-right` | `ChevronRight` | |
| `chevron-up` | `ChevronUp` | |
| `chevrons-up-down` | `UnfoldMore` | |
| `circle-check` | `CircleCheck` | |
| `circle-user` | `UserCircle` | |
| `circle-x` | `CircleX` | |
| `close` | `Cancel01` | two crossing strokes; `X` draws four from the centre |
| `copy` | `Copy` | |
| `cpu` | `Cpu` | |
| `dash` | `Minus` | |
| `delete` | `Delete02` | |
| `ellipsis` | `Ellipsis` | |
| `ellipsis-vertical` | `EllipsisVertical` | |
| `external-link` | `ExternalLink` | |
| `eye` | `Eye` | |
| `eye-off` | `EyeOff` | |
| `file` | `FileEmpty02` | same outline as `FileText`; `…01` folds the other corner |
| `file-text` | `FileText` | |
| `folder` | `Folder01` | |
| `folder-closed` | `FolderClosed` | |
| `folder-open` | `FolderOpen` | |
| `frame` | `Frame` | Lucide's `#`, not the corner brackets Phosphor used |
| `gallery-vertical-end` | `GalleryVerticalEnd` | |
| `github` | `Github` | |
| `globe` | `Globe02` | bare `Globe` sits on a desk stand |
| `hard-drive` | `HardDrive` | |
| `heart` | `Heart` | |
| `heart-off` | `HeartOff` | |
| `inbox` | `Inbox` | |
| `info` | `Info` | |
| `inspector` | `SquareDashedMousePointer` | |
| `layout-dashboard` | `LayoutDashboard` | |
| `loader` | `Loader` | |
| `loader-circle` | `LoaderCircle` | |
| `map` | `Map` | |
| `maximize` | `ArrowExpand01` | |
| `memory-stick` | `MemoryStick` | |
| `menu` | `Menu01` | bare `Menu` boxes the lines in |
| `minimize` | `ArrowShrink02` | |
| `minus` | `Minus` | |
| `moon` | `Moon02` | the crescent; bare `Moon` is a cratered sphere |
| `network` | `Network` | |
| `palette` | `Palette` | |
| `panel-bottom` | `PanelBottom` | |
| `panel-bottom-open` | `PanelBottomOpen` | |
| `panel-left` | `PanelLeft` | |
| `panel-left-close` | `PanelLeftOpen` | swapped; see below |
| `panel-left-open` | `PanelLeftClose` | swapped; see below |
| `panel-right` | `PanelRight` | |
| `panel-right-close` | `PanelRightClose` | |
| `panel-right-open` | `PanelRightOpen` | |
| `pause` | `Pause` | |
| `play` | `Play` | |
| `plus` | `Plus` | |
| `redo` | `Redo02` | the arc; bare `Redo` closes into a refresh circle |
| `redo-2` | `Redo03` | the u-turn |
| `replace` | `Replace` | |
| `resize-corner` | `ResizeField` | `Resize01`/`Resize02` are hand gestures |
| `rotate-cw` | `Refresh` | `RotateCw` and `RotateClockwise` are half-dashed |
| `search` | `Search01` | |
| `settings` | `Settings01` | |
| `settings-2` | `SlidersHorizontal` | |
| `sort-ascending` | `ArrowUpNarrowWide` | |
| `sort-descending` | `ArrowDownWideNarrow` | |
| `square-terminal` | `SquareTerminal` | |
| `star` | `Star` | |
| `star-fill` | `Star` | filled; see above |
| `star-off` | `StarOff` | |
| `sun` | `Sun03` | straight rays; `Sun01` uses dots, `Sun02` flourishes |
| `thumbs-down` | `ThumbsDown` | |
| `thumbs-up` | `ThumbsUp` | |
| `triangle-alert` | `TriangleAlert` | |
| `undo` | `Undo02` | |
| `undo-2` | `Undo03` | |
| `user` | `User` | |
| `window-close` | `Cancel01` | |
| `window-maximize` | `Square` | |
| `window-minimize` | `Minus` | |
| `window-restore` | `Copy01` | |

### Deliberate deviations from a literal name match

- **`panel-left-close` and `panel-left-open` take HugeIcons' opposite names.**
  HugeIcons' `PanelLeftOpen` draws a left-pointing chevron and its
  `PanelLeftClose` a right-pointing one, which is the reverse of Lucide. The
  chevron is what the user reads, so the artwork is matched to the Lucide name
  and the HugeIcons names are crossed. The right-hand pair agrees with Lucide
  and is not crossed.
- **`maximize` is `ArrowExpand01` but `minimize` is `ArrowShrink02`.** HugeIcons
  numbers the two families in opposite order: `ArrowExpand01` and
  `ArrowShrink02` share the top-right/bottom-left diagonal, and `…02` with `…01`
  share the other. Making both `01` would give a pair whose arrows run on
  different diagonals.
- **`delete` is a bin, not Lucide's backspace key.** `IconName::Delete` is what
  application code reaches for when it wants a destructive-delete affordance,
  and this set has no other bin glyph.
- **`arrow-*` uses the `…02` family.** `ArrowDown01` and the rest of the `…01`
  family are byte-identical to the chevrons, so using them would leave the app
  with no arrow at all.
- `close` and `window-close` share `Cancel01`; `dash`, `minus` and
  `window-minimize` share `Minus`; `star` and `star-fill` share `Star` at
  different fills. Lucide draws each of those pairs the same way too.
- `sort-ascending` and `sort-descending` use HugeIcons' arrow-plus-bars glyphs,
  which carry the sort direction in the bar widths as well as the arrow.
