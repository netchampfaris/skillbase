#!/usr/bin/env bash
#
# Builds Skillbase, restarts it in the background, and screenshots it — without
# ever taking keyboard focus from whatever the person at the keyboard is doing.
#
#   script/preview.sh                       # -> a PNG under $TMPDIR, path printed
#   script/preview.sh shots/list.png        # -> that path
#   SKILLBASE_HOME=/tmp/fakehome script/preview.sh
#
# Use this instead of driving the application on screen. Three things make it
# leave focus alone:
#
#   * The binary is wrapped in a .app bundle. A bare binary run from a terminal
#     never gets its window ordered in unless the application activates, so
#     `cargo run` cannot be made quiet — the bundle can.
#   * `open -g` launches it behind everything, and SKILLBASE_NO_ACTIVATE stops
#     the application calling `cx.activate(true)` on itself.
#   * `screencapture -l` reads the window's own buffer, so the window is
#     captured where it stands, occluded or not.
#
# The preview instance is left running, and the next run replaces it. Pass
# --stop to shut it down without taking a screenshot.
#
# Needs Screen Recording permission for the terminal, granted once in
# System Settings > Privacy & Security.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
app="$root/target/debug/Skillbase.app"
# A bundle built without --install symlinks the binary, so the running process
# reports the real path rather than the one inside the bundle.
binary="$root/target/debug/skillbase"
pidfile="$root/target/.preview-pid"

# Every running instance of *this checkout's* debug binary.
#
# Matching the command line is not enough: `pgrep -f` sees the path as it was
# typed, so an instance started as `./target/debug/skillbase` does not match the
# absolute path and survives. `lsof -d txt` reports the executable the kernel
# actually mapped, which is the same string however the process was launched,
# and tells this checkout apart from any other clone on the machine.
running_instances() {
    local pid
    for pid in $(pgrep -x skillbase || true); do
        if lsof -p "$pid" -a -d txt -Fn 2>/dev/null | grep -qxF "n$binary"; then
            echo "$pid"
        fi
    done
}

# Stops every instance, not just the one this script last started. A run that
# crashed, or one from an earlier shell, leaves an orphan behind, and a second
# window is worse than no window: `screenshot.sh` takes a pid, so the orphan
# photographs cleanly while the instance you meant to look at is somewhere else.
stop_previous() {
    local pids
    pids="$(running_instances)"
    if [ -n "$pids" ]; then
        # shellcheck disable=SC2086
        kill $pids 2>/dev/null || true
        for _ in $(seq 20); do
            [ -z "$(running_instances)" ] && break
            sleep 0.1
        done
        if [ -n "$(running_instances)" ]; then
            # shellcheck disable=SC2086
            kill -9 $pids 2>/dev/null || true
        fi
    fi
    rm -f "$pidfile"
}

if [ "${1:-}" = "--stop" ]; then
    stop_previous
    echo "preview stopped"
    exit 0
fi

out="${1:-}"
if [ -z "$out" ]; then
    out="$(mktemp -t skillbase-preview).png"
else
    mkdir -p "$(dirname "$out")"
fi

cargo build --manifest-path "$root/Cargo.toml"
"$root/script/bundle-macos.sh" debug >/dev/null

stop_previous

# `open` does not report the pid it started, so note which ones existed first.
before="$(running_instances)"
SKILLBASE_NO_ACTIVATE=1 open -g -n "$app"

pid=""
for _ in $(seq 40); do
    sleep 0.25
    for candidate in $(running_instances); do
        case " $before " in
            *" $candidate "*) ;;
            *) pid="$candidate" ;;
        esac
    done
    [ -n "$pid" ] && break
done

if [ -z "$pid" ]; then
    echo "preview: the application did not start" >&2
    exit 1
fi
echo "$pid" > "$pidfile"

# The process exists before its window does.
for _ in $(seq 40); do
    if swift "$root/script/window-id.swift" Skillbase "$pid" >/dev/null 2>&1; then
        break
    fi
    sleep 0.25
done

SCREENSHOT_PID="$pid" "$root/script/screenshot.sh" "$out"
