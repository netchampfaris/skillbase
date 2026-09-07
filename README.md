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

**Installs from GitHub, and says when there is an update.** Search skills.sh,
or point Skillbase at a repository and a directory inside it. What comes down
is pinned to an exact commit, and where it came from is written into the
skill's own `SKILL.md` under `metadata` — the same four keys `gh skill install`
writes, so the two tools understand each other. Skills that `npx skills`
installed are picked up too, by reading its lockfile.

Checking for updates compares the tree sha of one subdirectory rather than the
repository's last commit, so a skill is only out of date when the skill itself
changed. Checks batch by repository, and a repository that has not moved costs
a few hundred bytes. If you have edited a skill locally and it also changed
upstream, Skillbase says so and refuses until you choose which to keep.

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
other twelve agents record nothing, so a zero there means "not measurable"
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
git clone https://github.com/netchampfaris/skillbase.git
cd skillbase
cargo run
```

Rust 1.98 or newer. macOS and Linux.

## Installing it as an application

`cargo run` is fine while working on Skillbase, but it gives you a process, not
an application: no icon, and nothing for Spotlight or a launcher to find. To
install it properly:

```sh
cargo build --release
script/bundle-macos.sh release --install
```

That builds `~/Applications/Skillbase.app`, which is one of the directories
Spotlight and launchers such as Raycast and Alfred index, so "Skillbase" becomes
something you can type. `--install` copies the binary into the bundle, so the
application keeps working after `cargo clean` and after this repository moves.

Without `--install` the bundle is written to `target/<profile>/Skillbase.app`
and symlinks the binary instead, so a plain `cargo build` refreshes it. That is
the one to use while developing — but nothing indexes `target/`, so it will not
appear in a launcher.

Re-run the same command to update an installed copy. A launcher that still shows
a stale icon is caching it; the script re-registers the bundle, which usually
settles it.

## Application icon

The mark on the bundle, as distinct from the interface icon set below.
`assets/icon.png` is generated, not drawn by hand:

```sh
python3 script/make-icon.py
```

Edit the constants at the top of that script to change the mark, then re-run
the bundle script to rebuild the `.icns`. The `.icns` itself is a build
artifact and is not checked in.

## Testing against a throwaway home

Every path Skillbase reads or writes resolves under one home directory, and
`SKILLBASE_HOME` overrides it:

```sh
SKILLBASE_HOME=/tmp/fakehome cargo run
```

The title bar shows an orange badge naming the override, so a screenshot always
says which tree is live. Use this for anything destructive.

## Screenshots without stealing focus

Driving the application on screen to see a change interrupts whoever is at the
keyboard: the window comes to the front and takes the keyboard from whatever
they were doing. `script/preview.sh` builds, restarts and screenshots Skillbase
without ever doing that:

```sh
script/preview.sh                  # a PNG under $TMPDIR, path printed
script/preview.sh shots/list.png   # or a path you choose
script/preview.sh --stop           # shut the preview copy down
```

Three things keep it quiet. `SKILLBASE_NO_ACTIVATE=1` makes the window open with
`focus: false`, so AppKit orders it in without making it key, and the
`cx.activate` call is skipped. `open -g` launches it behind everything else.
`screencapture -l` then reads that window's own backing buffer, which works
while the window is behind others, on another Space, or never activated at all.

The preview copy is a separate process from any Skillbase you have open, and
each run replaces the one before it. It needs Screen Recording permission for
the terminal, granted once in System Settings.

One limit is worth knowing, because it is silent otherwise. macOS stops
compositing windows on an inactive Space, and a fullscreen application puts you
on a Space of its own — so while you are in one, the preview window is not being
drawn, and `screencapture` returns the last frame it painted rather than the
current one. That frame can be minutes old and still show a loading state that
finished long ago. Both scripts refuse to capture in that case rather than hand
back something stale; switch to the Space holding the window, or set
`SCREENSHOT_ALLOW_STALE=1` if an old frame is what you actually want.

`script/screenshot.sh` captures an already-running window on its own, and
`script/window-id.swift` prints the window id it uses.

## Layout

| Path            | What it holds                                                                   |
| --------------- | ------------------------------------------------------------------------------- |
| `crates/core`   | Discovery, the agent registry, install operations, `SKILL.md` parsing. No GPUI. |
| `src`           | The interface: sidebar, list, detail pane, settings.                            |
| `docs/SPEC.md`  | The design contract, including the full agent table.                            |
| `assets/themes` | Light and dark theme definitions.                                               |
| `assets/icons`  | The icon set and the agent brand marks, each with a `SOURCES.md`.               |

`crates/core` carries the tests. It has no dependency on the UI, so the
filesystem behaviour can be exercised without a window. Run them with:

```sh
cargo test --workspace
```

## Icons

The Lucide set bundled with `gpui-kit` is replaced wholesale by HugeIcons in its
stroke-rounded variant (`@hugeicons/core-free-icons` 4.3.0, MIT). Skillbase
registers its own `AssetSource`, which serves `assets/icons` and hands anything
it does not hold to the framework's; the 101 file names match one for one, so no
code changed. `assets/icons/SOURCES.md` records the mapping, including the few
places where the two sets disagree about which name goes with which glyph.

The agent logos in `assets/icons/agents` are SVG files from Simple Icons (CC0)
and lobe-icons (MIT), but the logos themselves are trademarks of their owners
and no trademark licence comes with the files. Skillbase uses each mark
nominatively, to label the agent it belongs to in a list of agents you have
installed. `assets/icons/agents/SOURCES.md` has the per-file detail.

## Not in v1

Project-scoped skills, publishing to a registry, and Windows.
