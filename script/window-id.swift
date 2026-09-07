// Prints the CoreGraphics window id of an application's document window.
//
//   swift script/window-id.swift [owner-name] [pid]
//
// With a pid, only that process's windows are considered, which is what tells
// a development build apart from an installed one — they carry the same owner
// name and the same size. With `--list`, every match is printed as
// `id<TAB>pid<TAB>WxH` instead.
//
// Reading the window list does not raise, focus, or otherwise disturb a
// window, which is the whole point: `screencapture -l` can then capture it
// while the person at the keyboard works in another application.

import CoreGraphics
import Foundation

var arguments = Array(CommandLine.arguments.dropFirst())
let listing = arguments.contains("--list")
// macOS stops compositing windows on an inactive Space, and `screencapture -l`
// then hands back the last frame that was painted, however old. Any capture
// meant to show current state has to know that, so this exits 3 when the
// window it found is not being composited.
let requireOnscreen = arguments.contains("--onscreen")
arguments.removeAll { $0 == "--list" || $0 == "--onscreen" }

let owner = arguments.first ?? "Skillbase"
let wantedPid = arguments.count > 1 ? Int(arguments[1]) : nil

// Deliberately not `.optionOnScreenOnly`. A window on another Space, or one
// belonging to an application that was never activated, reports itself as not
// on screen — and `screencapture -l` captures it perfectly well anyway,
// because it reads the window's backing buffer rather than the display.
let options: CGWindowListOption = [.excludeDesktopElements]
guard let windows = CGWindowListCopyWindowInfo(options, kCGNullWindowID) as? [[String: Any]]
else {
    FileHandle.standardError.write(Data("window-id: cannot read the window list\n".utf8))
    exit(1)
}

var matches: [(id: Int, pid: Int, width: Int, height: Int, area: Double, onscreen: Bool)] = []
for window in windows {
    guard window[kCGWindowOwnerName as String] as? String == owner,
        let id = window[kCGWindowNumber as String] as? Int,
        let pid = window[kCGWindowOwnerPID as String] as? Int,
        let bounds = window[kCGWindowBounds as String] as? [String: Any],
        let width = bounds["Width"] as? Double,
        let height = bounds["Height"] as? Double
    else { continue }

    if let wantedPid, pid != wantedPid { continue }
    // Menu bar extras, notification panels and other chrome share the owner
    // name. The document window is the one big in both directions; a 1512x33
    // menu bar strip clears a plain area threshold, so check each side.
    guard width >= 400 && height >= 300 else { continue }
    let onscreen = (window[kCGWindowIsOnscreen as String] as? Bool) ?? false
    matches.append((id, pid, Int(width), Int(height), width * height, onscreen))
}

if listing {
    for m in matches.sorted(by: { $0.area > $1.area }) {
        print("\(m.id)\t\(m.pid)\t\(m.width)x\(m.height)\t\(m.onscreen ? "onscreen" : "not-composited")")
    }
    exit(matches.isEmpty ? 2 : 0)
}

guard let best = matches.max(by: { $0.area < $1.area }) else { exit(2) }
if requireOnscreen && !best.onscreen {
    FileHandle.standardError.write(
        Data("window-id: window \(best.id) is not being composited\n".utf8))
    exit(3)
}
print(best.id)
