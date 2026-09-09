#!/usr/bin/env bash
# Lay out the built binary as a directory a person can unpack anywhere and run,
# and as something a desktop can list in its application menu.
#
#   script/bundle-linux.sh [debug|release] [triple] [name]
#
# `name` is the directory the payload is laid out in, and so the directory
# somebody gets when they unpack the tarball. It defaults to the triple, which
# is precise and not very friendly; the release workflow passes
# skillbase-<version>-linux-x86_64 instead.
#
# Prints the directory it built. The release workflow tars it up; there is
# nothing here that has to run on the machine the tarball ends up on.
#
# A release binary carries `assets/` inside it — `rust-embed` reads from disk in
# debug builds and embeds in release ones — so the payload is the executable
# plus the two files a desktop needs to show a name and an icon.
set -euo pipefail

PROFILE="${1:-release}"
TRIPLE="${2:-}"
NAME="${3:-}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
VERSION="${VERSION:-0.1.0}"

# `--target` puts the output under target/<triple>/<profile>; a build without
# one leaves it directly under target/<profile>.
if [ -n "$TRIPLE" ]; then
  BIN="$ROOT/target/$TRIPLE/$PROFILE/skillbase"
else
  BIN="$ROOT/target/$PROFILE/skillbase"
fi
[ -x "$BIN" ] || { echo "no binary at $BIN — build it first" >&2; exit 1; }

OUT="$ROOT/target/bundle/${NAME:-skillbase-$VERSION${TRIPLE:+-$TRIPLE}}"
rm -rf "$OUT"
mkdir -p "$OUT/bin" "$OUT/share/applications" "$OUT/share/icons/hicolor/512x512/apps"

cp "$BIN" "$OUT/bin/skillbase"
chmod +x "$OUT/bin/skillbase"
cp "$ROOT/assets/icon.png" "$OUT/share/icons/hicolor/512x512/apps/skillbase.png"

cat > "$OUT/share/applications/skillbase.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Skillbase
Comment=Manage Agent Skills across every coding agent on your machine
Exec=skillbase
Icon=skillbase
Terminal=false
Categories=Development;Utility;
StartupWMClass=skillbase
DESKTOP

# The tarball is unpacked wherever the person keeps such things, so the
# installer copies out of it rather than assuming a prefix. ~/.local is the one
# directory that needs no privileges and that every desktop already reads.
cat > "$OUT/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# Copy Skillbase into ~/.local, where the desktop and the shell both look.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
prefix="${PREFIX:-$HOME/.local}"

install -Dm755 "$here/bin/skillbase" "$prefix/bin/skillbase"
install -Dm644 "$here/share/applications/skillbase.desktop" \
  "$prefix/share/applications/skillbase.desktop"
install -Dm644 "$here/share/icons/hicolor/512x512/apps/skillbase.png" \
  "$prefix/share/icons/hicolor/512x512/apps/skillbase.png"

command -v update-desktop-database >/dev/null &&
  update-desktop-database "$prefix/share/applications" 2>/dev/null || true

echo "installed to $prefix"
case ":$PATH:" in
  *":$prefix/bin:"*) ;;
  *) echo "note: $prefix/bin is not on your PATH" ;;
esac
INSTALL
chmod +x "$OUT/install.sh"

echo "$OUT"
