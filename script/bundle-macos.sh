#!/usr/bin/env bash
# Wrap the built binary in a .app bundle so macOS treats Skillbase as a real
# application: its own name and icon in the menu bar and Dock, and a window
# that the window server can address.
#
#   script/bundle-macos.sh [debug|release]   build a bundle in target/<profile>
#   script/bundle-macos.sh release --install install it to ~/Applications
#   script/bundle-macos.sh release --target aarch64-apple-darwin --copy
#
# By default the binary is symlinked, so `cargo build` alone picks up a change
# and there is nothing to re-run. --install and --copy copy it instead: a
# bundle that leaves this checkout — installed under ~/Applications, or on its
# way into a disk image — has to keep working after `cargo clean`, and after
# this repository moves or goes away. --copy differs from --install only in
# leaving the bundle under target/.
#
# Launchers index ~/Applications and /Applications. They do not index target/,
# which is why a bundle left there does not show up in Spotlight or Raycast.
#
# MACOS_SIGN_IDENTITY names a Developer ID in the keychain to sign with. Left
# unset, the bundle is signed ad-hoc, which is enough for the machine that built
# it and not enough for anyone who downloads it.
set -euo pipefail

PROFILE="debug"
INSTALL=""
COPY=""
TRIPLE=""
while [ $# -gt 0 ]; do
  case "$1" in
    debug|release) PROFILE="$1" ;;
    --install) INSTALL="1"; COPY="1" ;;
    --copy) COPY="1" ;;
    --target) TRIPLE="${2:-}"; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# `--target` puts the output under target/<triple>/<profile>; a build without
# one leaves it directly under target/<profile>.
if [ -n "$TRIPLE" ]; then
  BIN="$ROOT/target/$TRIPLE/$PROFILE/skillbase"
else
  BIN="$ROOT/target/$PROFILE/skillbase"
fi
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
VERSION="${VERSION:-0.1.0}"

[ -x "$BIN" ] || { echo "no binary at $BIN — run cargo build${PROFILE:+ --$PROFILE} first" >&2; exit 1; }

if [ -n "$INSTALL" ]; then
  APP="$HOME/Applications/Skillbase.app"
  mkdir -p "$HOME/Applications"
else
  APP="$ROOT/target/${TRIPLE:+$TRIPLE/}$PROFILE/Skillbase.app"
fi

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Skillbase</string>
  <key>CFBundleDisplayName</key><string>Skillbase</string>
  <key>CFBundleIdentifier</key><string>dev.skillbase.app</string>
  <key>CFBundleExecutable</key><string>skillbase</string>
  <key>CFBundleIconFile</key><string>Skillbase</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
PLIST

# The icon is generated from assets/icon.png by script/make-icon.py; the
# .icns is a build artifact and is not checked in.
ICON_SRC="$ROOT/assets/icon.png"
if [ -f "$ICON_SRC" ]; then
  ICONSET="$(mktemp -d)/Skillbase.iconset"
  mkdir -p "$ICONSET"
  for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$ICON_SRC" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
    sips -z "$((size * 2))" "$((size * 2))" "$ICON_SRC" \
      --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
  done
  iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/Skillbase.icns"
  rm -rf "$(dirname "$ICONSET")"
else
  echo "warning: $ICON_SRC is missing, so the bundle has no icon" >&2
fi

if [ -n "$COPY" ]; then
  cp "$BIN" "$APP/Contents/MacOS/skillbase"
else
  ln -sf "$BIN" "$APP/Contents/MacOS/skillbase"
fi

# Ad-hoc signing: without it macOS caches the unsigned bundle's identity and
# keeps showing the old icon after a rebuild. A real identity, when one is in
# the keychain, is asked for by name and its failure is not swallowed — a
# release that quietly went out unsigned is worse than one that did not go out.
#
# --options runtime turns on the hardened runtime, which notarisation requires
# and which an ad-hoc signature has no use for.
if [ -n "${MACOS_SIGN_IDENTITY:-}" ]; then
  codesign --force --options runtime --timestamp \
    --sign "$MACOS_SIGN_IDENTITY" "$APP"
else
  codesign --force --sign - "$APP" >/dev/null 2>&1 || true
fi

# Nudge Launch Services, which is what Spotlight and Raycast read.
touch "$APP"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
  -f "$APP" >/dev/null 2>&1 || true

echo "$APP"
