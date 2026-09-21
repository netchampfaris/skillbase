# Development

## Build from source

Building needs Rust 1.98 or newer.

```sh
git clone https://github.com/netchampfaris/skillbase.git
cd skillbase
cargo run
```

On macOS, `cargo run` gives you a process, not an application. To get an icon
and something Spotlight can find:

```sh
cargo build --release
script/bundle-macos.sh release --install
```

That writes `~/Applications/Skillbase.app` with the binary copied in, so it
keeps working after `cargo clean` or after this checkout moves. Run the same
command again to update it.

Without `--install`, the bundle goes to `target/<profile>/Skillbase.app` and
symlinks the binary, so a plain `cargo build` refreshes it. Use that while
developing.

## Layout

The workspace has two crates.

| Path            | What it holds                                                                   |
| --------------- | ------------------------------------------------------------------------------- |
| `crates/core`   | Discovery, the agent registry, install operations, `SKILL.md` parsing. No GPUI. |
| `src`           | The interface: sidebar, list, detail pane, settings.                            |
| `script`        | Bundling, icon generation, and screenshot scripts.                              |
| `docs/SPEC.md`  | The design contract, including the full agent table.                            |
| `assets/themes` | The light and dark palettes.                                                    |
| `assets/icons`  | The icon set and the agent brand marks, each with a `SOURCES.md`.               |

The tests live in `crates/core`, which does not depend on the UI, so the
filesystem behavior runs without a window:

```sh
cargo test --workspace
```

A few tests talk to GitHub and are skipped by default:

```sh
cargo test -p skillbase-core --test live_github -- --ignored
```

## A throwaway home

Every path Skillbase reads or writes resolves under one home directory, and
`SKILLBASE_HOME` overrides it:

```sh
SKILLBASE_HOME=/tmp/fakehome cargo run
```

The title bar shows an orange badge naming the override. Use this for anything
destructive.

## Screenshots without stealing focus

Launching the app to look at a change brings its window to the front and takes
the keyboard from whoever is typing. `script/preview.sh` builds, restarts, and
captures Skillbase without doing that:

```sh
script/preview.sh                  # a PNG under $TMPDIR, path printed
script/preview.sh shots/list.png   # or a path you choose
script/preview.sh --stop           # shut the preview copy down
```

It launches the bundle with `open -g` and `SKILLBASE_NO_ACTIVATE=1`, then reads
the window's own buffer with `screencapture -l`, so the window can stay behind
everything else. The terminal needs Screen Recording permission, granted once
in System Settings. The script refuses to capture while the window is on an
inactive Space, because macOS would hand back a stale frame. The header of the
script has the details.

## Icons

The application icon, `assets/icon.png`, is generated:

```sh
python3 script/make-icon.py
```

The interface uses HugeIcons in its stroke-rounded variant, served from
`assets/icons` in place of the Lucide set bundled with `gpui-kit`. The agent
logos in `assets/icons/agents` come from Simple Icons and lobe-icons. They are
trademarks of their owners and are used only to label each agent in the list.
Both `SOURCES.md` files have the per-file detail.

## Releases

Merging into `main` publishes a release, unless the merge only changes
documentation, the workflows, or the developer scripts. The `paths-ignore` list
in the workflow is the exact rule. `.github/workflows/release.yml` builds
the four artifacts, tags the commit, and writes the notes from the pull
requests that landed since the last tag.

The version comes from `script/next-version.sh`, which takes the newest tag,
raises its patch number by one, and uses that unless `Cargo.toml` already
declares something larger. So an ordinary merge moves 0.1.3 to 0.1.4 and needs
no thought. To cut a feature version instead, set the version in `Cargo.toml`
to 0.2.0 in the pull request that earns it, and the manifest wins. The major
number is never raised by the workflow; 1.0.0 is a decision, and it is made the
same way, by editing the manifest.

Only a push to `main` publishes. A pull request that touches the release
machinery builds all four artifacts and attaches them to the run without
tagging anything, and so does a run started by hand from the Actions tab. That
is how a change to any of this gets tested before it is trusted.

Signing is ad-hoc until five repository secrets exist — `MACOS_CERTIFICATE`,
`MACOS_CERTIFICATE_PASSWORD`, `APPLE_ID`, `APPLE_APP_PASSWORD` and
`APPLE_TEAM_ID`. With them the workflow signs with the Developer ID in the
certificate and notarises the disk images, and the quarantine step in the README stops
being necessary. Nothing else has to change.
