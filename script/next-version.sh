#!/usr/bin/env bash
#
# The version the next release should carry, printed on stdout.
#
#   script/next-version.sh          # -> 0.1.4
#
# Two numbers decide it: the highest `v*` tag in the repository, and the
# version in Cargo.toml. The answer is whichever is greater of
#
#   * the newest tag with its patch number raised by one, and
#   * the version Cargo.toml already declares.
#
# So an ordinary merge moves 0.1.3 -> 0.1.4 and nothing else has to happen. To
# release a feature version instead, edit Cargo.toml to 0.2.0 and merge: the
# manifest wins because it is the larger of the two. The same is true of 1.0.0,
# which is why nothing here ever raises the major number on its own.
#
# With no tags at all the manifest stands unchanged, so the first release is
# whatever Cargo.toml says today.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# `[workspace.package]` comes first in the manifest, and the crate's own
# `[package]` inherits from it, so the first `version = ` line is the one.
manifest="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
[ -n "$manifest" ] || { echo "no version in Cargo.toml" >&2; exit 1; }

# `sort -V` orders 0.10.0 after 0.9.0, which `sort` on its own does not.
latest="$(git -C "$root" tag -l 'v*' | sed 's/^v//' | sort -V | tail -1)"

if [ -z "$latest" ]; then
    echo "$manifest"
    exit 0
fi

IFS=. read -r major minor patch <<<"$latest"
bumped="$major.$minor.$((patch + 1))"

printf '%s\n%s\n' "$bumped" "$manifest" | sort -V | tail -1
