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

**Edits skills in place.** `SKILL.md` opens in a syntax-highlighted editor with
its frontmatter parsed into name and description fields. An unmutated file
round-trips byte for byte: comments, quote style, block scalars and key order
all survive a save. A skill whose YAML does not parse still opens, so you can
fix it.

**Controls visibility per agent.** One switch per agent links or unlinks that
agent's directory. Codex additionally supports disabling a skill through
`~/.codex/config.toml`, and Claude Code through a `skills-disabled` directory;
Skillbase uses each agent's own mechanism and says which one it is using.

**Consolidates duplicates.** Copying a skill into eight agent directories is how
most people start, and those copies drift. Skillbase compares every duplicate
directory against the origin and replaces the identical ones with symlinks. A
duplicate whose content differs is refused and listed, because that difference
is an edit somebody made; overriding it takes one checkbox per directory.

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

`crates/core` carries the tests. It has no dependency on the UI, so the
filesystem behaviour can be exercised without a window.

## Not in v1

Editing bundled files, project-scoped skills, installing from a remote
registry, versioning, and Windows.
