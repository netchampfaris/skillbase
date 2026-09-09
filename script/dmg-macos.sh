#!/usr/bin/env bash
# Put a built .app into a disk image, which is how a macOS application is
# handed to someone who did not build it.
#
#   script/dmg-macos.sh <path/to/Skillbase.app> [out.dmg]
#
# Prints the path to the image. The window holds the application and a link to
# /Applications, so installing is the drag everyone already knows.
#
# APPLE_NOTARY_KEYCHAIN_PROFILE names a `notarytool` profile to submit the
# finished image under, and APPLE_NOTARY_KEYCHAIN the keychain holding it, for
# a profile that is not in the default one. Left unset, the image goes out
# unnotarised and macOS will refuse to open it on the first try — see the
# README.
set -euo pipefail

APP="${1:?usage: dmg-macos.sh <Skillbase.app> [out.dmg]}"
[ -d "$APP" ] || { echo "no bundle at $APP" >&2; exit 1; }

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
OUT="${2:-$ROOT/target/Skillbase-$VERSION.dmg}"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

# -R, not -r: the bundle may hold symlinks that a copy must not follow, and
# ditto is what Apple's own tooling uses to keep resource forks intact.
ditto "$APP" "$STAGE/$(basename "$APP")"
ln -s /Applications "$STAGE/Applications"

mkdir -p "$(dirname "$OUT")"
rm -f "$OUT"
# ULFO is lzfse-compressed and read-only, which is both smaller and faster to
# open than the zlib default, and is supported back to macOS 10.11.
hdiutil create -volname "Skillbase" -srcfolder "$STAGE" \
  -ov -format ULFO "$OUT" >/dev/null

if [ -n "${APPLE_NOTARY_KEYCHAIN_PROFILE:-}" ]; then
  # Spelled out twice rather than assembled in an array: bash 3.2 is what
  # /bin/bash still is on macOS, and it treats an empty array under `set -u`
  # as an unbound variable.
  if [ -n "${APPLE_NOTARY_KEYCHAIN:-}" ]; then
    xcrun notarytool submit "$OUT" --wait \
      --keychain-profile "$APPLE_NOTARY_KEYCHAIN_PROFILE" \
      --keychain "$APPLE_NOTARY_KEYCHAIN"
  else
    xcrun notarytool submit "$OUT" --wait \
      --keychain-profile "$APPLE_NOTARY_KEYCHAIN_PROFILE"
  fi
  # Stapling writes the ticket into the image, so it opens on a machine that
  # cannot reach Apple.
  xcrun stapler staple "$OUT"
fi

echo "$OUT"
