# Agent brand marks

Monochrome SVG marks for the coding agents Skillbase knows about, used as sidebar
icons. Every file is a single `<svg>` with `fill="currentColor"` and no hardcoded
colour, so gpui tints it with the element's text colour.

## Shape conventions

- Square `viewBox`. The artwork is optically centred and its longest dimension is
  85% of the box, so the marks sit at the same weight as Lucide at 24x24.
- No `width` / `height` attributes.
- No `<style>`, `<defs>`, `<mask>`, gradients, or embedded raster.
- Where a mark has holes (Kiro's eyes, Codex's inner glyphs), the holes are
  subpaths of one path with `fill-rule="evenodd"` on the path element.

## Per agent

| File | Source | Licence of the artwork file | Note |
| --- | --- | --- | --- |
| `claude-code.svg` | Simple Icons `claudecode`, upstream https://code.claude.com | CC0 1.0 (Simple Icons) | Claude Code starburst. Wide mark, so it fills the box horizontally. |
| `codex.svg` | lobe-icons `Codex/Mono`, https://github.com/lobehub/lobe-icons | MIT | OpenAI Codex mark, not the plain OpenAI blossom. |
| `cursor.svg` | Simple Icons `cursor`, upstream https://cursor.com/brand | CC0 1.0 (Simple Icons) | Cursor cube. |
| `gemini-cli.svg` | Simple Icons `googlegemini`, upstream https://gemini.google.com | CC0 1.0 (Simple Icons) | Gemini spark. Google has no separate Gemini CLI mark; the product mark is used. |
| `opencode.svg` | Simple Icons `opencode`, upstream https://github.com/anomalyco/opencode `packages/identity/mark.svg` | CC0 1.0 (Simple Icons) | opencode square. |
| `goose.svg` | lobe-icons `Goose/Mono`, https://github.com/lobehub/lobe-icons | MIT | Block's codename goose bird. Matches `block/goose` `ui/desktop/src/images/icon.svg`. |
| `amp.svg` | lobe-icons `Amp/Mono`, https://github.com/lobehub/lobe-icons | MIT | Amp's three-chevron symbol. See caveat below. |
| `copilot.svg` | Simple Icons `githubcopilot`, upstream https://primer.style/foundations/icons/copilot-24 | MIT (Primer Octicons) | GitHub Copilot face. |
| `zed.svg` | Simple Icons `zedindustries`, upstream https://github.com/zed-industries/zed `assets/icons/logo_96.svg` | CC0 1.0 (Simple Icons) | Zed spiral. |
| `cline.svg` | Simple Icons `cline`, upstream https://cline.bot/assets/branding/logos/cline-wordmark-black.svg | CC0 1.0 (Simple Icons) | Cline robot head. |
| `junie.svg` | lobe-icons `Junie/Mono`, https://github.com/lobehub/lobe-icons | MIT | JetBrains Junie. Geometry checked against https://www.jetbrains.com/img/banners-menu-main/junie.svg. |
| `warp.svg` | Simple Icons `warp`, upstream https://warp.dev | CC0 1.0 (Simple Icons) | Warp terminal mark. |
| `kiro.svg` | lobe-icons `Kiro/Mono`, https://github.com/lobehub/lobe-icons | MIT | AWS Kiro ghost. Eyes are evenodd holes, so it works as a flat silhouette. |
| `devin.svg` | lobe-icons `Devin/Mono`, https://github.com/lobehub/lobe-icons | MIT | Cognition Devin hexagon lattice. Geometry matches the inline hexagon on app.devin.ai. |

## Licensing

Two things are licensed separately here, and only one of them is covered by CC0
or MIT.

The **SVG files** come from Simple Icons (CC0 1.0, public domain dedication),
lobe-icons (MIT), and — for `copilot.svg` — GitHub Primer Octicons (MIT). Those
licences let this repository copy and modify the files, which is what was done:
each path was re-centred inside a new viewBox and recoloured to `currentColor`.

The **logos themselves are trademarks of their owners** — Anthropic, OpenAI,
Anysphere, Google, the opencode maintainers, Block, Amp, GitHub/Microsoft, Zed
Industries, Cline Bot, JetBrains, Warp, Amazon, and Cognition. No trademark
licence comes with the SVG files, and neither Simple Icons nor lobe-icons can
grant one.

Skillbase uses them **nominatively**: each mark labels the agent it belongs to in
a list of agents the user has installed. That is the ordinary use for an app like
this and does not imply that any of these vendors endorse or sponsor Skillbase.
It is not a licence, though. If a vendor's brand guidelines forbid this use, or
if Skillbase is ever redistributed under someone else's branding, the affected
mark should be dropped.

Two marks worth flagging specifically:

- **Amp** ships no square symbol on ampcode.com — its own app icon is the "amp"
  wordmark on a gradient, which is illegible at 24px. `amp.svg` is lobe-icons'
  monochrome symbol for the product, not an asset published by Amp.
- **Kiro** and **Devin** were both redrawn by lobe-icons rather than published by
  AWS and Cognition as monochrome SVGs. They are faithful, but they are third-party
  renditions.

## Rejected sources

Recorded so nobody re-treads these:

- Simple Icons `amp` is **AMP / Accelerated Mobile Pages**, not Amp by Sourcegraph.
  Do not use it.
- Simple Icons `jetbrains` is the JetBrains company logo, not Junie.
- Simple Icons has no `zed` slug; the Zed editor is under `zedindustries`.
- Simple Icons has no `codex`, `devin`, `kiro`, `goose`, or `junie` slug.
- `https://ampcode.com/app-icon.svg` is a gradient wordmark with `<defs>`, a drop
  shadow filter, and four `linearGradient` / `radialGradient` fills. Unusable.
- `https://kiro.dev/icon.svg` is a purple rounded rect with a white ghost and black
  eyes, plus a luminance `<mask>`. Would collapse to a solid blob when tinted.
- `https://cognition.ai/icon.svg` is Cognition's corporate mark, not Devin's, and
  carries an inline `<style>` block and an `feInnerShadow` filter.
- `https://devin.ai/` returns HTTP 429 to scripted requests; the Devin geometry was
  cross-checked against an inline SVG in the `https://app.devin.ai/` document instead.
