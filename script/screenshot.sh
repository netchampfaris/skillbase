#!/usr/bin/env bash
#
# Screenshots Skillbase without raising it or taking keyboard focus.
#
#   script/screenshot.sh                    # -> a PNG under $TMPDIR, path printed
#   script/screenshot.sh shots/list.png     # -> that path
#   SCREENSHOT_APP="Some Other App" script/screenshot.sh
#   SCREENSHOT_PID=1234 script/screenshot.sh    # one instance of several
#   SCREENSHOT_ALLOW_STALE=1 script/screenshot.sh   # capture anyway (see below)
#
# `screencapture -l` reads the window's own backing buffer, so this works while
# the window sits behind other windows and never pulls it in front. Use this
# rather than driving the application on screen: the person at the keyboard is
# usually in the middle of something else.
#
# It refuses to capture a window macOS is not compositing. A window on an
# inactive Space — which is where yours will be whenever you are in a fullscreen
# application — stops being drawn, and `screencapture -l` then returns the last
# frame it painted, which can be minutes old and show a loading state that has
# long since finished. Reading a stale frame as if it were current is worse than
# getting no frame at all, so that case is an error. `SCREENSHOT_ALLOW_STALE=1`
# overrides it when you genuinely want the last known frame.
#
# The window has to exist. To start one without it stealing focus:
#
#   open -g -a Skillbase                                  # the installed bundle
#   SKILLBASE_NO_ACTIVATE=1 cargo run                      # a development build
#
# Requires Screen Recording permission for the terminal, granted once in
# System Settings > Privacy & Security.

set -euo pipefail

app="${SCREENSHOT_APP:-Skillbase}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

out="${1:-}"
if [ -z "$out" ]; then
    out="$(mktemp -t skillbase-shot).png"
else
    mkdir -p "$(dirname "$out")"
fi

require=--onscreen
[ -n "${SCREENSHOT_ALLOW_STALE:-}" ] && require=

set +e
id="$(swift "$here/window-id.swift" "$app" ${SCREENSHOT_PID:-} $require 2>/dev/null)"
status=$?
set -e

if [ "$status" -eq 3 ]; then
    echo "screenshot: '$app' has a window, but macOS is not compositing it." >&2
    echo "  Its Space is not the active one, so the only frame available is a" >&2
    echo "  stale one. Switch to the Space holding the window, or re-run with" >&2
    echo "  SCREENSHOT_ALLOW_STALE=1 if an old frame is genuinely what you want." >&2
    swift "$here/window-id.swift" "$app" --list >&2 2>/dev/null || true
    exit 3
fi

if [ "$status" -ne 0 ] || [ -z "$id" ]; then
    echo "screenshot: '$app' has no window." >&2
    echo "  start the installed bundle:  open -g -a $app" >&2
    echo "  or a development build:      SKILLBASE_NO_ACTIVATE=1 cargo run" >&2
    exit 1
fi

# -l <id>  that window only, occluded or not
# -o       no drop shadow, so the image is exactly the window
# -x       no shutter sound
screencapture -l "$id" -o -x "$out"

if [ ! -s "$out" ]; then
    echo "screenshot: capture produced nothing. Screen Recording permission?" >&2
    exit 1
fi

echo "$out"
