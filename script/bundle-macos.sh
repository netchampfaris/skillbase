#!/usr/bin/env bash
# Wrap the built binary in a .app bundle so macOS treats Skillbase as a real
# application: its own name in the menu bar and Dock, and a window that the
# window server can address. The binary is symlinked, so `cargo build` alone is
# enough to pick up a change — no need to re-run this script.
set -euo pipefail

PROFILE="${1:-debug}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/$PROFILE/skillbase"
APP="$ROOT/target/$PROFILE/Skillbase.app"

[ -x "$BIN" ] || { echo "no binary at $BIN — run cargo build first" >&2; exit 1; }

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Skillbase</string>
  <key>CFBundleDisplayName</key><string>Skillbase</string>
  <key>CFBundleIdentifier</key><string>dev.skillbase.app</string>
  <key>CFBundleExecutable</key><string>skillbase</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>0.1.0</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
PLIST

ln -sf "$BIN" "$APP/Contents/MacOS/skillbase"
touch "$APP"
echo "$APP"
