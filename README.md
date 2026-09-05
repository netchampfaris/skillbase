# Skillbase

A desktop app for managing [Agent Skills](https://agentskills.io) across every
coding agent on your machine.

Every agent looks for skills somewhere different — `~/.claude/skills`,
`~/.codex/skills`, `~/.cursor/skills`, `~/.config/goose/skills`, and so on.
Skillbase treats that as an implementation detail. A skill has one directory
holding its bytes; every other path an agent reads is a symlink into it. Which
agents can see a skill becomes a row of switches rather than a directory tree
you maintain by hand.

## What it does

**Finds what is already there.** Scanning is read-only and covers 16 known
locations, including the vendor-neutral `~/.agents/skills` that most agents now
read natively. Nothing moves until you ask for it.

**Edits skills in place.** The detail pane is a set of tabs. `SKILL.md` has one
of its own, a syntax-highlighted editor over the whole file, and the Overview
tab has its frontmatter as name and description fields; both save through the
same write. An unmutated file round-trips byte for byte: comments, quote style,
block scalars and key order all survive a save. A skill whose YAML does not
parse still opens, so you can fix it.

**Opens the rest of the directory too.** The Overview lists the skill directory
as a tree. Click any text file — a reference, a script, a template — and it
opens in a tab with its own editor, saved back verbatim. Markdown, YAML, TOML,
shell and Python are highlighted; anything else opens as plain text. Images and
other binaries are listed but not opened.

**Controls visibility per agent.** One switch per agent links or unlinks that
agent's directory. Codex additionally supports disabling a skill through
`~/.codex/config.toml`, and Claude Code through a `skills-disabled` directory;
Skillbase uses each agent's own mechanism and says which one it is using.

**Consolidates duplicates.** Copying a skill into eight agent directories is how
most people start, and those copies drift. Skillbase compares every duplicate
directory against the origin and replaces the identical ones with symlinks. A
duplicate whose content differs is refused and listed, because that difference
is an edit somebody made; overriding it takes one checkbox per directory.

**Counts what you actually use.** The list sorts by name or by how often a
skill has been invoked, and the choice sticks between runs. Only Claude Code
and GitHub Copilot CLI leave a machine-readable record of an invocation — the
other thirteen agents record nothing, so a zero there means "not measurable"
rather than "unused" — and Claude Code prunes its transcripts after 30 days, so
the figure is recent history rather than a lifetime total. The sort menu says
which agents were counted; Settings gives the per-source numbers and the
directories they came from. The first count walks a few hundred megabytes on a
background thread; after that it resumes from a cache in
`~/.skillbase/usage.json` and takes milliseconds.

**Leaves other tools alone.** A skill whose directory is not in Skillbase's
store is *unmanaged*: readable and editable in place, but its visibility is
read-only until you explicitly adopt it. If another tool fans your skills out,
Skillbase will not fight it.

## Build

```sh
cargo run
```

Rust 1.98 or newer. macOS and Linux.

For a macOS `.app` bundle (needed for a Dock icon and a proper bundle identity):

```sh
script/bundle-macos.sh
open target/debug/Skillbase.app
```

The bundle symlinks the binary, so a plain `cargo build` refreshes it.

## Testing against a throwaway home

Every path Skillbase reads or writes resolves under one home directory, and
`SKILLBASE_HOME` overrides it:

```sh
SKILLBASE_HOME=/tmp/fakehome cargo run
```

The title bar shows an orange badge naming the override, so a screenshot always
says which tree is live. Use this for anything destructive.

## Layout

| Path                | What it holds                                              |
| ------------------- | ---------------------------------------------------------- |
| `crates/core`       | Discovery, the agent registry, install operations, `SKILL.md` parsing. No GPUI. |
| `src`               | The interface: sidebar, list, detail pane, settings.        |
| `docs/SPEC.md`      | The design contract, including the full agent table.        |
| `assets/themes`     | Light and dark theme definitions.                           |
| `assets/icons`      | The icon set and the agent brand marks, each with a `SOURCES.md`. |

`crates/core` carries the tests. It has no dependency on the UI, so the
filesystem behaviour can be exercised without a window.

## Icons

The Lucide set bundled with `gpui-kit` is replaced wholesale by Phosphor duotone
(`@phosphor-icons/core` 2.1.1, MIT). Skillbase registers its own `AssetSource`,
which serves `assets/icons` and hands anything it does not hold to the
framework's; the 101 file names match one for one, so no code changed. Ten
glyphs have no Phosphor equivalent and are drawn by hand in the same geometry.
`assets/icons/SOURCES.md` records the mapping.

The agent logos in `assets/icons/agents` are SVG files from Simple Icons (CC0)
and lobe-icons (MIT), but the logos themselves are trademarks of their owners
and no trademark licence comes with the files. Skillbase uses each mark
nominatively, to label the agent it belongs to in a list of agents you have
installed. `assets/icons/agents/SOURCES.md` has the per-file detail.

## Not in v1

Project-scoped skills, installing from a remote registry, versioning, and
Windows.
