# Skillbase — Specification

A native desktop application for macOS and Linux that manages Agent Skills
across every coding agent installed on the machine.

Status: this is the design contract for v1. Anything marked **Later** is out of
scope for the first working version.

---

## 1. What an Agent Skill is

A skill is a **directory**. It contains `SKILL.md` at its root and may contain
bundled files beside it (`scripts/`, `references/`, `assets/`).

`SKILL.md` is Markdown with YAML frontmatter:

```
---
name: pdf-processing
description: Extract text and tables from PDF files. Use when the user
  mentions PDFs, forms, or scanned documents.
license: Apache-2.0
metadata:
  version: "1.2"
---

# PDF Processing

Instructions the agent reads when the skill fires...
```

Two fields are required by the open specification at `agentskills.io`:

| Field | Rule |
| --- | --- |
| `name` | kebab-case, 64 characters or fewer, must match the directory name |
| `description` | 1024 characters or fewer |

`license`, `compatibility`, `metadata`, and `allowed-tools` are optional and
standard. Individual agents add their own fields — Claude Code alone recognises
`when_to_use`, `disable-model-invocation`, and `context: fork`. Skillbase must
treat every field it does not recognise as data to be preserved exactly, never
as an error and never as something to drop.

**Round-trip fidelity is the first requirement of this application.** A user
who opens a skill, changes its description, and saves must get back a file
identical to the original except for that description.

---

## 2. Where skills live

Fourteen agents now read the vendor-neutral directory `~/.agents/skills/`
directly. It is the closest thing the ecosystem has to a standard location.
Others keep their own directory and read `~/.agents/skills/` as a fallback, or
not at all.

### 2.1 Scopes Skillbase knows about

| Scope | Global path | Project path | Reads `~/.agents/skills`? |
| --- | --- | --- | --- |
| Shared | `~/.agents/skills/` | `.agents/skills/` | — it *is* the shared location |
| Claude Code | `~/.claude/skills/` | `.claude/skills/` | no — needs its own entry |
| Codex | `~/.codex/skills/` | `.agents/skills/` | yes |
| Cursor | `~/.cursor/skills/` | `.cursor/skills/` | yes |
| Gemini CLI | `~/.gemini/skills/` | `.gemini/skills/` | yes |
| opencode | `~/.config/opencode/skills/` | `.opencode/skills/` | yes |
| Goose | `~/.config/goose/skills/` | `.goose/skills/` | yes |
| Amp | `~/.config/agents/skills/` | `.agents/skills/` | yes |
| Copilot / VS Code | `~/.copilot/skills/` | `.github/skills/` | yes |
| Zed | `~/.agents/skills/` | `<worktree>/.agents/skills/` | yes, trusted worktrees only |
| Cline | `~/.cline/skills/` | `.cline/skills/` | no |
| JetBrains Junie | `~/.junie/skills/` | `.junie/skills/` | yes |
| Warp | `~/.warp/skills/` | `.warp/skills/` | yes |
| Kiro | `~/.kiro/skills/` | `.kiro/skills/` | yes |
| Devin Desktop | `~/.devin/skills/` | `.devin/skills/` | yes |
| Factory Droid | — | `.agents/skills/` | yes |
| Replit Agent | — | `.agents/skills/` | yes |

The registry is **data, not code** — a table Skillbase ships and can update
without a new release, because this list changed substantially during 2026
(Windsurf became Devin Desktop, Roo Code shut down, Continue.dev was archived
without ever supporting the format).

### 2.2 Agents deliberately excluded

Aider, Continue.dev, and Cline's `.clinerules` use flat prose files with a
different shape entirely. Symlinking a skill directory into those locations
produces nothing useful. Skillbase does not pretend to support them. Claude
Desktop's chat surface stores skills server-side as uploaded archives and has
no filesystem presence, so it is out of reach by construction.

Naming them in the UI as unsupported is better than silently omitting them —
a user who wonders "where's Aider?" deserves an answer.

---

## 3. The model

### 3.1 Origin and links

Every skill on disk has exactly one **origin**: the real directory holding the
actual bytes. Any other path for the same skill is a **link** — a symlink
pointing at the origin, or a copy of it.

Skillbase discovers all of them and groups them by skill name. A skill's
**visibility** is the set of agents that can currently reach it, computed from
where its origin and links sit.

### 3.2 Managed and unmanaged skills

A skill is **managed** when its origin is inside Skillbase's own store at
`~/.skillbase/store/<name>/`. Managed skills get full visibility control,
because Skillbase owns the origin and can add or remove links freely.

Every other skill is **unmanaged**. Skillbase shows it, reads it, and edits it
in place, but reports its visibility read-only. An explicit **Adopt** action
moves the origin into the store and leaves a symlink behind at the old path,
after which the skill is managed. Nothing about adoption is automatic.

This matters because a machine may already be running another skill manager.
Moving 56 directories out from under a tool the user relies on, without asking,
would be indefensible. Adoption is per skill, opt-in, and reversible by a
**Release** action that moves the origin back.

### 3.3 What a visibility toggle does

For a managed skill, turning an agent on creates a relative symlink from that
agent's directory to the store. Turning it off removes the link. Turning on
**Shared** creates the link in `~/.agents/skills/`, which reaches every agent in
the table above that reads it — so Shared is presented as one switch, with the
agents it covers listed beneath.

Symlinks, not copies, are the default: a copy goes stale the moment the skill is
edited, and the whole point of this application is that one edit lands
everywhere. Where an agent is known not to resolve symlinks, the registry marks
it `copy` and Skillbase copies instead, then re-copies on every save.

The UI states the filesystem consequence of each toggle in plain words. A user
should never have to guess whether a switch wrote to their disk.

### 3.4 Disabling without uninstalling

Some agents distinguish "present" from "active":

- **Claude Code** — move the link to `~/.claude/skills-disabled/`, the
  convention already in use on machines that do this.
- **Codex** — set `enabled = false` in the skill's `[[skills.config]]` entry in
  `~/.codex/config.toml`, leaving the file where it is.

Where an agent has no such mechanism, disabled means the link is removed, and
the UI says so rather than implying a distinction that does not exist.

**Later:** opencode's wildcard `permission.skill` deny rules.

### 3.5 How often a skill has been invoked

Two of the fifteen agents leave a machine-readable record of a skill being used:

- **Claude Code** — one JSON object per line under `~/.claude/projects/`. A
  skill ran when an `assistant` row holds a `tool_use` block named `Skill`, and
  when a `user` row's content is a slash-command envelope. Those two signals do
  not overlap, and a slash command emits no tool call at all, so counting only
  the first badly undercounts.
- **GitHub Copilot CLI** — a `skill.invoked` event under
  `~/.copilot/session-state/`.

The other thirteen record nothing: not sparsely, and not in a different shape.
A count of zero for a skill installed only into those agents means "not
measurable", not "not used", and the interface has to say which it means.
Codex's shell history holds `cat …/SKILL.md` calls. That is not used as a
signal: it counts file reads, which happen during greps, during editing and
during Skillbase's own scans, and it over-counts real use by roughly ten times.

Claude Code prunes its transcripts at about 30 days, so the counts are a
rolling window rather than lifetime totals, and legitimately go down between
two loads. `Usage::sources` reports which sources were read and how many files
each contributed, so the interface can qualify the number instead of presenting
it as a fact about all time; `Usage::is_empty` distinguishes "nothing on this
machine records usage" from a column of honest zeros, which `Usage::count`
cannot.

`Usage::load` is blocking and belongs on a background task. A first load reads
several hundred megabytes across a few hundred session files, which takes about
400ms in a release build; each line is first tested for a byte substring, so
only the few hundred that could be about a skill reach a JSON parser. It then
writes each file's length, modification time and counts to
`~/.skillbase/usage.json`, and a later load reads only the bytes each file has
grown by, in a few milliseconds. A missing, corrupt or differently versioned
cache means "scan everything" rather than an error. Nothing else is written.

---

## 4. Architecture

```
skillbase/
├── crates/core/          skillbase-core — no GUI dependency
│   ├── doc.rs            SKILL.md parse / serialize, round-trip fidelity
│   ├── frontmatter.rs    ordered frontmatter, unknown fields preserved
│   ├── skill.rs          a skill on disk, bundled files
│   ├── slug.rs           name validation and slugify
│   ├── error.rs          parse errors vs. validation issues
│   ├── registry.rs       the agent table from §2.1
│   ├── discovery.rs      scan scopes, group by name, resolve origin vs. link
│   ├── install.rs        link, unlink, adopt, release, disable
│   └── usage.rs          invocation counts from agent session records
└── src/                  the GPUI application
    ├── main.rs           window, theme, root
    ├── theme.rs          the Codex-derived palette
    ├── accent.rs         the operating system's accent colour
    ├── assets.rs         the icon set, served ahead of the framework's
    ├── app.rs            top-level view and state
    └── ui/               sidebar, list, detail, settings, editor
```

The core crate has no GPUI dependency, so its tests run in seconds rather than
waiting on a graphics stack. Every filesystem decision lives there and is
testable without a window.

**Framework:** `gpui-kit` 0.6 is the single dependency. GPUI is reached through
it (`use gpui_kit::*`), never listed directly. Rust 1.98 or newer, edition 2024.

### 4.1 Rules the application code follows

These come from the framework's own coding guides and are not negotiable:

- No raw hex, `rgb()`, or `hsla()` in application code. Colors resolve from
  `cx.theme()` by semantic role; the palette is defined once in `theme.rs`.
- Element identity derives from the skill name, never from a list index.
- Business logic and filesystem work stay out of `render`.
- Stateful components (`InputState`, `EditorState`) are held as `Entity<T>` on
  the view, not rebuilt each frame.

---

## 5. Interface

A title bar over three panes, following the Codex desktop idiom: a sidebar of
scopes, a list of skills, and a detail pane. The detail pane is where editing
happens; there is no fourth pane.

```
┌─────────────────────────────────────────────────────────────────┐
│  <  Skillbase │ All  57                    New skill   Refresh  │
├────────────┬─────────────────────┬──────────────────────────────┤
│  Library   │  Search      [Sort] │  skill-name  Managed  [Save] │
│  All       │ ─────────────────── │ ──────────────────────────── │
│  Shared    │  code-review        │  Overview  SKILL.md  api.md  │
│  Managed   │  Reviews a diff…    │  ────────                    │
│  Unmanaged │                     │  Name                        │
│  Invalid   │  pdf-processing     │  ┌───────────────────────┐   │
│  Conflicts │  Extracts text…     │  └───────────────────────┘   │
│            │                     │  Files                       │
│  Agents    │  writing-style      │   SKILL.md                   │
│  Claude    │  House style…       │   references/                │
│  Codex     │                     │     api.md                   │
│  …         │                     │                              │
│            │                     │  › Visible to   Shared, +11  │
│  Settings  │                     │                              │
└────────────┴─────────────────────┴──────────────────────────────┘
```

The title bar carries the sidebar's colour and no hairline beneath it, so the
two read as one surface running under the traffic lights. It holds what belongs
to the window rather than to any one pane: the sidebar toggle, the name
Skillbase, a hairline, the current scope and how many skills it lists, and then,
pushed to the right, the `SKILLBASE_HOME` badge when that variable is set, New
skill, and Refresh. Those last two sat in the list header, where they read as
list controls: Refresh re-scans every scope, and a new skill lands in the store
whichever scope is selected.

### 5.1 Sidebar

Fixed width, collapsible to icons. Two groups:

- **Library** — All, Shared, Managed, Unmanaged, Invalid, Conflicts. Each shows
  a count.
- **Agents** — one row per agent detected on the machine, each carrying that
  agent's own logo, with the number of skills it can currently see. Agents not
  installed are hidden, not greyed; a "Show all agents" toggle in Settings
  reveals them.

Rows within a group sit 2px apart, because they are one navigation surface
rather than a stack of separate controls. The separation is the groups' job:
they sit 16px apart. Collapsed to icons there is no room for the group labels,
so a hairline between the two runs of icons carries that boundary instead.

Settings pins to the bottom, in the footer as a bare menu rather than wrapped in
a `SidebarFooter`. That wrapper adds its own padding on top of the footer's,
which indents the row past every row above it and, once the sidebar collapses to
48px, leaves the item no width at all.

### 5.2 Skill list

One row per skill: name in medium weight, description truncated to one line in
muted text beneath it. A skill whose frontmatter fails validation carries a
warning marker, and one with duplicate directories a copy marker. The agents
that can see a skill are not on the row — the detail pane's "Visible to" header
answers that in one line, and a row of agent tags under every skill was three
lines of chrome for a question the reader is rarely asking at that moment.

Above the list, a search field filtering on name and description as the user
types, and a menu holding the two orderings:

| Ordering | Rule |
| --- | --- |
| Name | Alphabetical. The default. |
| Most used | Most invocations first, ties broken by name. Each row then shows its count on the right, or an em dash for zero. |

The count is shown only under Most used, where it is what the order is based on.
The menu's last line says where the numbers come from — which agents were
counted, or that nothing on this machine records them (§3.5) — because a number
that quietly means less than it looks like is worse than no number. The choice
is written to `~/.skillbase/settings.json` under `sort`, as `name` or
`most-used`.

### 5.3 Detail pane

A header, a tab strip, and whichever tab is active.

The header holds the skill name, a Managed or Unmanaged tag, and three
controls: Reveal, which opens the origin directory in the file manager; Delete;
and Save, disabled until the active tab has an unsaved change.

The tab strip sits directly under the header. It is a `TabBar` in the underline
variant — the boxed default marks the selected tab by weight alone against this
theme, and a second band of chrome under the header would compete with it —
with `menu(true)`, so tabs that do not fit the pane become a dropdown rather
than being clipped off the right edge.

#### The two permanent tabs

**Overview** and **`SKILL.md`** are always present and neither can be closed.
`SKILL.md` gets a tab of its own because every skill has that file and editing
it is the most common thing done in this pane; it used to sit at the bottom of a
single scrolling column, past the description, the switches and the duplicate
list. The tab is the `body` editor filling the pane, so the file scrolls rather
than the pane growing to fit it.

The Overview holds, in order:

1. **A banner**, when the file could not be read, or when its frontmatter does
   not parse. A skill that does not parse still opens; the banner says that the
   Name and Description fields stay empty until it is fixed in the `SKILL.md`
   tab.
2. **Name** and **Description** as real form fields, because they are the two
   fields the specification requires and the two an agent uses to decide
   whether to load the skill. Editing them writes back into the frontmatter
   without disturbing any other key.
3. **Files** — the skill directory, described below.
4. **Visible to**, collapsed. Editing a file is what this pane is mostly for,
   and an open column of a dozen switches pushed everything else off the
   bottom of the window. The closed header still answers the question the
   section exists to answer: a one-line summary of reach, naming the first
   three and counting the rest — "Shared, Claude Code, Codex, +11". Open, it is
   the Shared switch and then one flat row per agent, each carrying the agent's
   mark, its name, what a switch there will do on disk, and two fixed-width
   lanes: an **Enabled** switch and a **Visible** switch. Enabled appears only
   for the agents that distinguish present from active (§3.4), and its lane
   stays empty otherwise, so Visible lands in the same column on every row.
   Selecting a different skill closes the section again; re-showing the same
   skill after a toggle leaves it as the user left it.
5. **Duplicate copies** — the skill's other directories, compared against the
   origin.
6. **Location** — the origin path, whether it is managed, and Adopt or Release.

Field validation problems appear inline under the field that caused them, not
as a banner. The two banners above are for the file as a whole, which belongs to
no field.

#### Files

The skill directory listed as rows, replacing the row of tags that named the
bundled files without letting the user do anything with them. The listing is
depth-first: `SKILL.md` first whatever sorting would otherwise do with it, then
directories before files and alphabetical within each, which is the order a file
manager shows. A row carries a folder or file icon and indents one step per
level.

The listing goes three levels deep (`MAX_TREE_DEPTH`) and stops at 200 entries
(`MAX_TREE_ENTRIES`), so a directory pointed at something enormous degrades into
a truncated list rather than a hung pane. Past 19rem — about a dozen rows — the
list scrolls in place instead of pushing the sections below it off the tab.
Dotfiles are skipped: a dotfile in a skill directory belongs to an editor or to
version control, not to the skill.

A directory row is a heading, not a control; there is nothing to open, and a
disclosure triangle over a listing this short would hide files behind a click
for no gain. A file whose extension is on the binary denylist — 21 image, video,
audio, archive, font and document extensions — is marked "not text" and is not
clickable, because a row that looks clickable and then refuses is worse than one
that says why. Clicking any other file opens it.

#### File tabs

Opening a file adds a closable tab with its own editor. A tab is labelled by
file name alone, because `references/browse-the-web.md` across a tab leaves room
for nothing else; the full relative path comes back only when two open files
would otherwise carry the same label, which is the one case where the short form
does not identify the file. A dot on a tab marks unsaved edits, in a fixed lane
so the strip does not shuffle under the pointer as the user types. Closing a tab
with unsaved edits asks first. Closing the active tab returns to the Overview
rather than to a neighbouring tab: which neighbour is arbitrary, and the
Overview holds the file list. Selecting a different skill closes every file tab,
because those files belong to the skill that was showing.

#### Saving

Save writes whatever the active tab holds. The Overview and the `SKILL.md` tab
both save through the one existing path: they are two views of one file, the
Overview editing its frontmatter and the tab its text, and that single write
reconciles them. Any other tab writes its editor's text to that file verbatim —
nothing parses it and nothing rewrites it, because a bundled file is whatever
the skill's author put there.

#### Syntax highlighting

An editor can only highlight a grammar compiled into the binary, so the
extensions that highlight are exactly the `tree-sitter-*` features in the root
`Cargo.toml`:

| Extension | Grammar |
| --- | --- |
| `.md`, `.markdown`, `.mdx` | Markdown |
| `.yaml`, `.yml` | YAML |
| `.toml` | TOML |
| `.sh`, `.bash`, `.zsh` | Bash |
| `.py` | Python |

Every other file opens as plain text. Naming a grammar that is not compiled in
would not fail, it would silently produce plain text, so the two lists are kept
in step deliberately — and plain text stays readable, whereas the wrong grammar
does not.

### 5.4 Actions

| Action | Behaviour |
| --- | --- |
| New skill | Name and description, then a templated `SKILL.md` in the store |
| Install from folder | Pick a directory containing `SKILL.md`, copy into the store |
| Duplicate | Copy under a new name |
| Reveal in Finder | Open the origin directory |
| Delete | Remove the origin and every link, with a confirmation naming both counts |
| Refresh | Re-scan every scope |

Delete is the only destructive action and the only one that confirms. Toggling
visibility is reversible in one click and does not.

---

## 6. Theme

The palette derives from measurements of the Codex desktop app, expressed as a
`gpui-kit` theme file so the application code names roles rather than colors.

Light mode is built on three flat neutrals stacked without borders between
them, which is the single most characteristic thing about the reference design:

| Role | Light | Dark | Use |
| --- | --- | --- | --- |
| `background` | `#FFFFFF` | `#181818` | Main content pane |
| `sidebar` | `#F1F0F0` | `#1E1E1E` | Sidebar, running up under the traffic lights |
| `title_bar` | `#F1F0F0` | `#1E1E1E` | Continuous with the sidebar — no seam |
| `list_active` | `#E5E5E5` | `#2A2A2D` | Selected row |
| `list_hover` | `#F4F4F4` | `#242424` | Hover |
| `group_box` | `#F8F8F8` | `#242424` | Cards, code blocks |
| `border` | `#E0E0E0` | `#2E2E2E` | Hairlines, used sparingly |
| `foreground` | `#171A1D` | `#EDEDED` | Primary text |
| `muted_foreground` | `#8A8B8D` | `#8A8A8A` | Descriptions, counts, timestamps |
| `primary` | `#171A1D` | `#EDEDED` | Filled buttons |
| `success` | `#0BA43F` | `#4ADE80` | Valid, installed |
| `warning` | `#E0611F` | `#FB923C` | Validation problems |
| `danger` | `#C0392B` | `#F87171` | Delete |
| `ring` | `#989898` | `#8C8C8C` | Focus ring — a fallback the system accent replaces |

Dark values marked as estimates in the source research are exactly that; they
are chosen to preserve the relationships rather than to claim a measurement
that was never taken.

The four roles that carry the interaction accent — `ring`, `selection`,
`drag_border` and `drop_target` — hold that grey in the file only as a fallback.
`theme::follow_accent` reads macOS's `NSColor.controlAccentColor`, resolved
against the current light or dark appearance rather than whatever `NSApp`
currently believes, and points all four at it. It has to run after every
`Theme::sync_system_appearance`, not only at startup: syncing re-applies the
stored `ThemeConfig`, which restores the file's grey. On a machine left on the
default Multicolour setting AppKit resolves the accent to `#007AFF`. Off macOS
there is no such setting, so the grey stands — it is macOS's own Graphite
accent, which makes the fallback a colour the platform already uses rather than
an invention.

Typography follows the reference: the system UI font throughout, 13px sidebar
rows, 11–12px section headers in sentence case — **not** uppercase and not
letter-spaced — 14px body, and the system monospace at 13px for the editor.

Spacing uses the framework's semantic scale (2, 4, 8, 12, 16, 24, 32).

### 6.1 Principles carried over

- Color is a signal, never decoration. Neutral chrome; every hue means something.
- Hierarchy comes from background steps, not borders. Borders only where an
  interactive container or a data card needs an edge, and then barely visible.
- One accent hue with one job, and on macOS it is the system's rather than one
  Skillbase chose.
- Dense rows, generous horizontal padding. Compact vertically, never cramped.
- Follow the platform. Native traffic lights inset over a sidebar that runs
  beneath them, via `TitleBar::window_options()`.

### 6.2 Icons

The framework generates its `IconName` enum from the 101 Lucide filenames it
bundles, but resolves the bytes behind each name through whatever `AssetSource`
the application registered, and there is one source per application with no
chaining. `src/assets.rs` therefore registers a source that serves
`assets/icons/**` and delegates anything it does not hold to
`gpui_kit::assets::Assets`. That directory holds a Phosphor icon
(`@phosphor-icons/core` 2.1.1, MIT) under every one of those 101 names, so the
enum keeps Lucide's vocabulary — `Search`, `TriangleAlert` — while the artwork
is Phosphor throughout, and no call site changed.

The weight is duotone, and it survives GPUI's SVG pipeline. GPUI rasterises an
SVG to an alpha mask and tints it with one colour, discarding every hue in the
file; Phosphor's duotone weight is one
colour at two opacities, so the secondary path's `opacity="0.2"` comes through
the mask as 20% alpha of the theme colour, in light and dark alike. Ten names
have no Phosphor equivalent and are drawn by hand in Phosphor's geometry.
`assets/icons/SOURCES.md` records every mapping, every deliberate deviation, and
every hand-drawn file.

Agent brand marks are separate. They live in `assets/icons/agents/<id>.svg`, are
not part of the generated enum, and are reached by path through
`Icon::empty().path(…)`. Fourteen agents have one; anything else falls back to a
generic glyph rather than asking the renderer for a file that is not there,
which would paint nothing at all. They are monochrome by the same necessity as
the icons: a logo that depends on more than one hue arrives as a silhouette.

---

## 7. Verification

The application is driven on screen and screenshotted at each stage, not merely
compiled. The specific things to confirm visually:

- The window opens with traffic lights inset over a continuous sidebar surface.
- Real skills from this machine appear in the list, with correct counts, and the
  title bar names the scope they belong to.
- Selecting a skill loads its content into the editor with highlighting.
- Editing the description and saving changes only that line on disk.
- A visibility toggle creates or removes exactly the expected symlink.
- Light and dark both render correctly.

Filesystem behaviour is covered by tests in the core crate: round-trip fidelity,
discovery against a synthetic tree of agent directories, and link, adopt, and
release operations against a temporary directory.

Nothing writes outside `~/.skillbase/` and the agent directories the user has
explicitly targeted. Discovery is read-only.

---

## 8. Out of scope for v1

Editing bundled files, project-scoped skill management, browsing or installing
from a remote registry, skill versioning and diffing, import from an archive,
and Windows support. Each is plausible later; none is needed for the
application to be useful.
