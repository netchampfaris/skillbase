#!/usr/bin/env bash
#
# Write a version into the manifest and the lock file.
#
#   script/set-version.sh 0.1.4
#
# Cargo would rewrite the lock itself on the next build, but the release builds
# run with `--locked` so that a release is made of exactly the dependencies
# that were reviewed. That leaves the two files to be kept in step here.
#
# Both crates carry the workspace version, so both entries in the lock move.
# The lock is edited by package name rather than by position: a dependency two
# hundred lines away also has a `version = ` line, and only the one under
# `name = "skillbase"` is ours.
set -euo pipefail

version="${1:?usage: set-version.sh <x.y.z>}"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
    echo "not a version: $version" >&2
    exit 2
}

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# `[workspace.package]` is the first table in the manifest and the only one
# with a bare `version = `, so the first match is the one to change. The rest
# of the file says `version.workspace = true`, or names a dependency's version
# inside an inline table, and neither starts a line with `version = `.
awk -v v="$version" '
    !done && /^version = "/ { sub(/"[^"]*"/, "\"" v "\""); done = 1 }
    { print }
' "$root/Cargo.toml" > "$root/Cargo.toml.tmp"
mv "$root/Cargo.toml.tmp" "$root/Cargo.toml"

# In the lock, `version` is the line after `name`, so remembering the last name
# seen is enough to know whose version this is.
awk -v v="$version" '
    /^name = "/ { name = $3 }
    /^version = "/ && (name == "\"skillbase\"" || name == "\"skillbase-core\"") {
        sub(/"[^"]*"/, "\"" v "\"")
    }
    { print }
' "$root/Cargo.lock" > "$root/Cargo.lock.tmp"
mv "$root/Cargo.lock.tmp" "$root/Cargo.lock"

echo "$version"
