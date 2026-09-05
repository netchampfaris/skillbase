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
│   └── install.rs        link, unlink, adopt, release, disable
└── src/                  the GPUI application
    ├── main.rs           window, theme, root
    ├── theme.rs          the Codex-derived palette
    ├── app.rs            top-level view and state
    └── ui/               sidebar, list, detail, editor
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

Three panes, following the Codex desktop idiom: a sidebar of scopes, a list of
skills, and a detail pane. The detail pane is where editing happens; there is no
fourth pane.

```
┌────────────┬─────────────────────┬──────────────────────────────┐
│            │  Search             │  skill-name          [Save]  │
│  All       │ ─────────────────── │ ──────────────────────────── │
│  Shared    │  code-review        │  Description                 │
│  Managed   │  Reviews a diff…    │  ┌────────────────────────┐  │
│  Unmanaged │  Claude · Codex     │  │                        │  │
│            │                     │  └────────────────────────┘  │
│  AGENTS    │  pdf-processing     │                              │
│  Claude    │  Extracts text…     │  Visible to                  │
│  Codex     │  Shared             │  [x] Shared   [ ] Claude     │
│  Cursor    │                     │  [ ] Codex    [ ] Cursor     │
│  …         │  writing-style      │                              │
│            │  House style…       │  SKILL.md                    │
│            │  Claude             │  ┌────────────────────────┐  │
│  Settings  │                     │  │ # Code Review          │  │
│            │                     │  │ …                      │  │
└────────────┴─────────────────────┴──────────────────────────────┘
```

### 5.1 Sidebar

Fixed width, collapsible to icons. Two groups:

- **Library** — All, Shared, Managed, Unmanaged, Invalid. Each shows a count.
- **Agents** — one row per agent detected on the machine, with the number of
  skills it can currently see. Agents not installed are hidden, not greyed;
  a "Show all agents" toggle in Settings reveals them.

Settings pins to the bottom, separated from the scrolling list.

### 5.2 Skill list

One row per skill: name in medium weight, description truncated to one line in
muted text beneath it, and a trailing row of the agents that can see it. A
skill whose frontmatter fails validation carries a warning marker.

Search filters on name and description as the user types.

### 5.3 Detail pane

Top: the skill name, and a Save button that is disabled until something changes.

Then, in order:

1. **Name and description** as real form fields, because they are the two
   fields the specification requires and the two an agent uses to decide
   whether to load the skill. Editing them writes back into the frontmatter
   without disturbing any other key.
2. **Visible to** — the Shared switch, then one switch per detected agent, each
   labelled with what it will do.
3. **SKILL.md** — the full file in a code editor with Markdown syntax
   highlighting, for everything the form fields do not cover.
4. **Bundled files** — a list of what else is in the directory, read-only in v1,
   with a button to reveal the folder in the file manager.
5. **Location** — the origin path, whether it is managed, and Adopt or Release.

Validation problems appear inline against the field that caused them, not as a
banner.

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
| `ring` | `#AB7AF7` | `#B18CFF` | Focus ring — the one accent hue |

Dark values marked as estimates in the source research are exactly that; they
are chosen to preserve the relationships rather than to claim a measurement
that was never taken.

Typography follows the reference: the system UI font throughout, 13px sidebar
rows, 11–12px section headers in sentence case — **not** uppercase and not
letter-spaced — 14px body, and the system monospace at 13px for the editor.

Spacing uses the framework's semantic scale (2, 4, 8, 12, 16, 24, 32).

### 6.1 Principles carried over

- Color is a signal, never decoration. Neutral chrome; every hue means something.
- Hierarchy comes from background steps, not borders. Borders only where an
  interactive container or a data card needs an edge, and then barely visible.
- One accent hue with one job.
- Dense rows, generous horizontal padding. Compact vertically, never cramped.
- Follow the platform. Native traffic lights inset over a sidebar that runs
  beneath them, via `TitleBar::window_options()`.

---

## 7. Verification

The application is driven on screen and screenshotted at each stage, not merely
compiled. The specific things to confirm visually:

- The window opens with traffic lights inset over a continuous sidebar surface.
- Real skills from this machine appear in the list, with correct agent badges.
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
