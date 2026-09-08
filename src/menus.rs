//! The application menu bar, and the lifecycle a Mac application is expected
//! to have.
//!
//! A GPUI application starts with no menu bar at all, which on macOS means
//! none of the commands the system reserves — Quit, Hide, Close, Minimize —
//! reach the application. Every one of them is an action here, bound to its
//! customary keystroke and listed in the menu it belongs to.
//!
//! The Edit menu is the exception: its items dispatch the editing actions the
//! framework's text controls already handle, so the menu drives whichever
//! input has focus rather than duplicating what it does.

use gpui_kit::component::input;
use gpui_kit::{App, KeyBinding, Menu, MenuItem, Window, actions};

actions!(
    skillbase,
    [
        About,
        BringAllToFront,
        CheckForUpdates,
        ClearMarks,
        CloseWindow,
        DeleteSkill,
        Hide,
        HideOthers,
        InstallFromGitHub,
        LinkMarked,
        MarkAll,
        Minimize,
        FindSkill,
        NewSkill,
        Quit,
        ReloadSkills,
        RevealInFinder,
        Save,
        ShowAll,
        ShowDiscover,
        ShowSettings,
        ToggleSidebar,
        UnlinkMarked,
        UpdateSkill,
        Zoom,
    ]
);

/// Install the menu bar, its shortcuts, and the handlers behind them.
pub fn init(cx: &mut App) {
    // The menu bar is built from a snapshot of the keymap: each item is
    // labelled with the shortcut bound to its action at the moment
    // `set_menus` runs, so the bindings have to be registered first.
    //
    // The editing actions are left alone. `gpui-base` already binds them in
    // its `Input` context, and the menu finds those bindings on its own.
    cx.bind_keys([
        KeyBinding::new("cmd-,", ShowSettings, None),
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new("cmd-alt-h", HideOthers, None),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-n", NewSkill, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        // Editing SKILL.md is what the detail pane is for, and in anything that
        // edits text Cmd-S is reflex: the hand reaches for it before the eye
        // finds the button.
        KeyBinding::new("cmd-s", Save, None),
        // What Finder, Notes and Music use for the same command. Cmd-Ctrl-S
        // is Mail's, and reads as a variant of Save rather than of Show.
        KeyBinding::new("cmd-alt-s", ToggleSidebar, None),
        KeyBinding::new("cmd-r", ReloadSkills, None),
        KeyBinding::new("cmd-f", FindSkill, None),
        KeyBinding::new("cmd-m", Minimize, None),
        // What the Finder binds for the same two gestures: Cmd-Delete removes
        // the selected thing, Cmd-Shift-R opens a folder. The skill list is
        // keyboard-navigable, so the row the arrows land on is the row these
        // act on.
        //
        // Delete is bound outside text controls only. Inside one, Cmd-Delete
        // deletes to the start of the line, and a keystroke that means "trim
        // this line" in every other application must not mean "delete this
        // skill" here. `!Input` is false at every depth once an `Input` is
        // anywhere on the focus path, so the editor keeps the key.
        KeyBinding::new("cmd-backspace", DeleteSkill, Some("!Input")),
        KeyBinding::new("cmd-shift-r", RevealInFinder, None),
        // Marking a set of rows is what Cmd-A means inside a list, and Escape
        // is what takes the set back to one. Both are scoped to the list's own
        // context, so Cmd-A still selects text in every field and Escape still
        // dismisses whatever overlay is on top. Registered here, before
        // `set_menus`, because the menu bar is built from a snapshot of the
        // keymap and an item whose binding lands later shows no shortcut.
        KeyBinding::new("cmd-a", MarkAll, Some(crate::ui::list::CONTEXT)),
        KeyBinding::new("escape", ClearMarks, Some(crate::ui::list::CONTEXT)),
    ]);

    // These act on the application or on the front window rather than on
    // anything the view owns, so they are registered globally. A global
    // handler also keeps the menu item enabled: macOS asks whether an action
    // is available before it draws the item, and an action nothing claims is
    // drawn greyed out.
    cx.on_action(|_: &About, _: &mut App| about::show());
    cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
    cx.on_action(|_: &Hide, cx: &mut App| cx.hide());
    cx.on_action(|_: &HideOthers, cx: &mut App| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx: &mut App| cx.unhide_other_apps());
    cx.on_action(|_: &BringAllToFront, cx: &mut App| cx.activate(true));
    cx.on_action(|_: &Minimize, cx: &mut App| {
        with_active_window(cx, |window| window.minimize_window())
    });
    cx.on_action(|_: &Zoom, cx: &mut App| with_active_window(cx, |window| window.zoom_window()));
    cx.on_action(|_: &CloseWindow, cx: &mut App| {
        with_active_window(cx, |window| window.remove_window())
    });

    cx.set_menus(menus());

    // GPUI keeps running after its last window goes away, which would leave
    // Cmd-W with a process still in the Dock and no way back to it. Skillbase
    // is a single-window application, so the last window closing is the user
    // saying they are done.
    cx.on_window_closed(|cx, _| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();
}

/// Run something against the window the command was aimed at.
///
/// The Window menu acts on whichever window is in front, and a global action
/// handler is only handed the `App`, so the window has to be looked up.
///
/// The lookup has to wait, though. A menu command arrives while GPUI is
/// already updating the window it was dispatched to, and a second update of
/// the same window from inside the first one fails rather than nesting, which
/// would leave the command silently doing nothing. Deferring runs it once
/// that update has finished.
fn with_active_window(cx: &mut App, f: impl FnOnce(&mut Window) + 'static) {
    cx.defer(move |cx| {
        if let Some(window) = cx.active_window() {
            window.update(cx, |_, window, _| f(window)).ok();
        }
    });
}

fn menus() -> Vec<Menu> {
    vec![
        // AppKit draws the first menu under the application's own name and
        // ignores the one given here, so what the menu bar reads is whatever
        // `CFBundleName` says: "Skillbase" from the bundle
        // `script/bundle-macos.sh` builds, and the executable's filename for a
        // bare `cargo run`. The name is still set to match, so the two agree
        // wherever the menu is read back rather than drawn.
        Menu::new("Skillbase").items([
            MenuItem::action("About Skillbase", About),
            MenuItem::separator(),
            MenuItem::action("Settings…", ShowSettings),
            MenuItem::separator(),
            MenuItem::action("Hide Skillbase", Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit Skillbase", Quit),
        ]),
        // Everything that puts a skill on this machine, changes it, or takes it
        // off again. The detail pane offers the same commands as buttons; these
        // are what makes them findable by reading the menu bar, and what gives
        // the keyboard a way to act on the row the arrow keys landed on.
        Menu::new("File").items([
            MenuItem::action("New Skill", NewSkill),
            MenuItem::action("Install from GitHub…", InstallFromGitHub),
            MenuItem::separator(),
            // "Close", not "Close Window": with a bundled file open in a tab,
            // this closes that tab first.
            MenuItem::action("Close", CloseWindow),
            // Below Close, where every document application on macOS puts it.
            MenuItem::action("Save", Save),
            MenuItem::separator(),
            MenuItem::action("Reveal in Finder", RevealInFinder),
            MenuItem::action("Update Skill", UpdateSkill),
            MenuItem::separator(),
            // The commands that act on every marked row rather than on the
            // selected one. Both linking items open a dialog naming the agent,
            // so both take the ellipsis. Delete Skill below acts on the marked
            // set too when there is one, which is why it keeps its place at the
            // foot of the menu rather than gaining a second item.
            MenuItem::action("Mark All", MarkAll),
            MenuItem::action("Clear Marks", ClearMarks),
            MenuItem::action("Link Marked Skills…", LinkMarked),
            MenuItem::action("Unlink Marked Skills…", UnlinkMarked),
            MenuItem::separator(),
            MenuItem::action("Delete Skill", DeleteSkill),
        ]),
        // Every item here belongs to the framework's text controls. The
        // search field and the detail pane's editor both answer them, and
        // macOS greys an item out whenever the focused control does not, so
        // the menu reports what is actually possible.
        Menu::new("Edit").items([
            MenuItem::action("Undo", input::Undo),
            MenuItem::action("Redo", input::Redo),
            MenuItem::separator(),
            MenuItem::action("Cut", input::Cut),
            MenuItem::action("Copy", input::Copy),
            MenuItem::action("Paste", input::Paste),
            MenuItem::action("Select All", input::SelectAll),
        ]),
        Menu::new("View").items([
            // Find belongs with the list it searches rather than in Edit.
            // The framework's own search action only answers while a text
            // control already has focus, which is no use as the way in to the
            // search field.
            MenuItem::action("Find Skill", FindSkill),
            MenuItem::action("Discover", ShowDiscover),
            MenuItem::separator(),
            MenuItem::action("Toggle Sidebar", ToggleSidebar),
            MenuItem::action("Reload Skills", ReloadSkills),
            MenuItem::action("Check for Updates", CheckForUpdates),
        ]),
        // Named "Window" so AppKit adopts it: it appends the window list and
        // keeps the checkmark on the front window without being asked.
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Zoom),
            MenuItem::separator(),
            MenuItem::action("Bring All to Front", BringAllToFront),
        ]),
    ]
}

/// The standard About panel.
///
/// AppKit builds it from the bundle's `Info.plist` — the name and version
/// `script/bundle-macos.sh` writes, and the icon beside them — so there is
/// nothing to pass and nothing for the application to lay out. Run outside a
/// bundle there is no plist to read and the panel names the executable
/// instead, which is the same degradation the menu bar's own title already
/// has.
#[cfg(target_os = "macos")]
mod about {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;

    pub(super) fn show() {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        // The panel opens behind everything if the application is not brought
        // forward first, which happens whenever About is chosen from the menu
        // bar of an application that is not already frontmost.
        app.activate();
        app.orderFrontStandardAboutPanel(None);
    }
}

#[cfg(not(target_os = "macos"))]
mod about {
    pub(super) fn show() {}
}
