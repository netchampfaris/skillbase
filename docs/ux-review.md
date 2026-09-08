# Skillbase user stories

Written from the user's point of view. Each story is a thing someone sits down
to do, not a feature.

## A. Getting started

A1. As a new user, I open Skillbase for the first time and see the skills I
    already have on disk, without having to import anything.
A2. As a new user with no skills at all, I see an empty state that tells me
    where to get one, not a blank pane.
A3. As a user, I understand which agents Skillbase found installed on my
    machine and which it did not.
A4. As a cautious user, I can tell that scanning changed nothing on disk.

## B. Installing

B1. As a user, I install one skill from a GitHub repository by pasting its URL.
B2. As a user, I search a registry (skills.sh) and install a skill from the
    result without knowing its repository URL.
B3. As a user, I install several skills in one go: I pick more than one search
    result, or paste a repo that holds many skills, and install them together.
B4. As a user, I install a skill from a subdirectory of a repo, and from a
    branch or tag that is not the default.
B5. As a user installing, I see progress, and I can cancel a download that is
    taking too long or that I started by mistake.
B6. As a user, when an install fails (network, 404, private repo, rate limit, a
    directory with no SKILL.md) I am told what failed and what to do about it.
B7. As a user, when the skill I am installing already exists, I am told, and I
    choose between replacing it, keeping both, or cancelling.
B8. As a user, after installing I land on the new skill, and I can immediately
    choose which agents see it.
B9. As a user, I install a skill from a local directory I already have.
B10. As a user with a private repo, I authenticate once and install from it.

## C. Removing

C1. As a user, I delete a skill and every symlink to it goes away with it.
C2. As a user, I am asked to confirm a delete, and told what will be removed
    and from where.
C3. As a user, I select several skills and delete them in one action.
C4. As a user, I can undo a delete I did not mean, or at least be told where
    the files went.
C5. As a user, I remove a skill from one agent without deleting the skill.

## D. Visibility per agent

D1. As a user, I turn one agent's access to a skill on or off with one switch.
D2. As a user, I give a skill to every agent, or take it from every agent, in
    one action rather than fourteen switches.
D3. As a user, I select several skills and enable them for one agent at once.
D4. As a user, when a switch cannot be honoured (agent not installed, a real
    file is in the way, no write permission) I am told why, not left with a
    switch that silently flips back.
D5. As a user, I see at a glance, from the list, which agents each skill
    reaches.

## E. Finding things

E1. As a user with a hundred skills, I search by name and by description.
E2. As a user, I filter the list to one agent, to a scope, or to skills that
    have an update.
E3. As a user, I sort by name or by how often I have used a skill, and I
    understand what a zero count means.
E4. As a user, I move through the list and open a skill with the keyboard.
E5. As a user, when a search returns nothing, I am told so and offered the next
    step (search the registry instead).

## F. Editing

F1. As a user, I edit SKILL.md and save it, and nothing else in the file
    changes.
F2. As a user, I edit the name and description as fields, without touching YAML.
F3. As a user, I am warned before I close or navigate away from unsaved edits.
F4. As a user, I open other files in the skill directory and save them.
F5. As a user, a skill with broken YAML still opens so I can repair it.
F6. As a user, I create a new skill from scratch inside Skillbase.
F7. As a user, I reveal the skill in Finder or open it in my editor.

## G. Updates

G1. As a user, I am told when an installed skill has a newer version upstream.
G2. As a user, I update one skill, and I update every out-of-date skill at once.
G3. As a user, when I have local edits and there is an upstream change, I am
    told what the conflict is and I choose.
G4. As a user, I check for updates when I want to, and I know when the last
    check happened.
G5. As a user, I see what changed before I accept an update.

## H. Duplicates

H1. As a user, I am told that copies of the same skill are scattered across
    agent directories.
H2. As a user, I consolidate the identical copies into symlinks in one action.
H3. As a user, I see which copies differ from the original, and what the
    difference is, before I overwrite them.

## I. Trust and recovery

I1. As a user, before anything is written to disk I know what will change.
I2. As a user, when an operation fails halfway, I am told the state it left
    behind.
I3. As a user, I can undo the last destructive action, or find the backup.
I4. As a user, I try Skillbase against a throwaway home without risking my real
    one, and the window makes that mode obvious.

## J. The window itself

J1. As a user, the app remembers my window size, selection, sort and filters
    between launches.
J2. As a user, every action I can take with the mouse has a menu item and a
    keyboard shortcut.
J3. As a user, long operations never freeze the window.
J4. As a user, the app follows my system appearance and accent colour.

---

# Gaps found against these stories

Reviewed 2026-09-08 against the code, story by story. Ranked by how much each
one costs a real user.

## Data loss and dead ends

1. **Unsaved edits vanish without a prompt (F3).** Selecting another skill
   clears the editor and resets the dirty flag; Cmd-Q and Cmd-W do the same.
   The only warning anywhere is on closing a bundled file tab.
2. **Delete is permanent (C4, I3).** `remove_dir_all` on the skill directory,
   no trash, no backup, no undo, and the confirmation lists counts rather than
   the paths it is about to remove.
3. **A half-finished operation reports the error and hides the damage (I2).**
   `delete` and `consolidate` drop the changes they already made when they hit
   an error, so "Permission denied" can mean six symlinks are already gone.
4. **Reinstalling a skill you already have is a dead end (B7).** The refusal is
   a raw absolute path and nothing else. Replace and rename both exist in the
   installer and neither is offered.
5. **An install shows nothing and cannot be stopped (B5).** Up to two minutes
   of silence, no progress, no cancel, and in Discover every row's button greys
   out so the user cannot see which skill is downloading.
6. **The default branch is guessed as `main` (B1, B4).** Every repo on `master`
   fails with an error naming a ref the user never typed, though the real
   default branch is one API call away.
7. **The window can fail to appear (J3).** The GitHub token lookup blocks the
   main thread before the first frame, for a value that frame does not use.

## Work the app makes the user repeat

8. **No multi-select (C3, D3).** One skill at a time, forever: clearing out
   twenty stale skills is twenty confirm cycles, and provisioning a new agent
   means opening every skill to flip the same switch.
9. **A repo full of skills installs nothing (B3).** Pasting a monorepo returns
   "has no SKILL.md", though the enumeration it needs already exists.
10. **No "update all" (G2).** Settings counts the skills behind upstream and
    offers no way to take them.
11. **No "all agents on/off" (D2).** Eight separate clicks, each a background
    write and a full rescan.

## Things the app knows and does not show

12. **Reach is invisible from the list (D5).** The central fact of the app
    appears only in one collapsed header of the detail pane.
13. **No filter for skills with an update (E2).**
14. **An update is described by seven hex characters (G5).** No diff, no
    changed-file list, no link to the upstream comparison — while the
    duplicates flow, in the same file, does this properly.
15. **"You have local edits" never says which (G3).** The user ticks a box
    agreeing to lose work they were never shown.
16. **A shared skill's switches contradict the same screen (C5, D1).** The
    header says Cursor sees the skill; Cursor's row shows Off, and there is no
    control that takes it away from Cursor alone.
17. **A zero use count reads as "never used" (E3).** Only two of fourteen
    agents record invocations at all.

## Rough edges

18. Cmd-F, type, Down does not reach the list (E4); a mouse click leaves the
    list unfocused.
19. Delete, Reveal and Update have no menu item and no shortcut (J2); Cmd-W
    quits the app when the user meant to close a file tab.
20. The New skill dialog closes and discards what was typed when the name is
    invalid (F6).
21. An empty search offers only "clear the search", never "look on skills.sh"
    (E5).
22. No way to install a skill from a local folder (B9).
23. Renaming a skill still means hand-editing YAML (F2).
24. Nothing about the window survives a relaunch except the sort order (J1).
25. The system accent colour reaches the focus ring and nothing else, and a
    Yellow, Green or Orange accent shows nowhere (J4).
26. Adopt and Release move directories on one click, explained only in a
    tooltip (I1).
27. A throwaway `SKILLBASE_HOME` is marked by a 12px triangle in the sidebar,
    which Cmd-Alt-S hides (I4).
28. Uninstalled agents are filtered out of the visibility list with nothing
    saying so, and a permission failure renders as an errno (D4).
29. Duplicate rows say which files differ, never how, and cannot be revealed
    (H3).
30. Failures that disappear: the install-record cache, the usage cache, the
    preferences read, three rollback paths, and the staging cleanup.

---

# What was fixed

Each item below was implemented and is covered by the test suite unless noted.
Numbers refer to the gaps above.

## Data loss and dead ends

- **1. Unsaved edits.** Switching skills, changing scope, Cmd-W and Cmd-Q all
  ask before discarding, offering Save, Discard and Cancel; the save is real
  and the gesture that would cost the work is named in the sentence.
- **2. Delete is recoverable.** A removed directory is moved to
  `~/.skillbase/trash/<name>-<timestamp>` instead of being destroyed, and the
  notification names where it went. The confirmation lists every path — origin,
  links and copies — rather than counts. A replacing install and an update go
  the same way.
- **3. Partial failures report the damage.** `delete` and `consolidate` carry
  the changes they made alongside the error, so a notification reads "removed
  the link …, then it stopped: …". The same treatment covers the visibility
  switches, including the link-all loop, and both rollback paths.
- **4. Reinstalling offers a choice.** Replace, keep both under the next free
  name, or cancel — and the dialog says where the existing copy is and whether
  it came from the same repository.
- **5. An install names itself and can be stopped.** A progress strip with the
  skill's name, "N of M" for a batch, and a Cancel that leaves nothing
  half-written. Only the row being installed is disabled.
- **6. The default branch is resolved**, not guessed, and recorded as resolved.
- **7. The window appears immediately.** The GitHub token lookup moved off the
  first frame.

## Work the app no longer repeats

- **8. Multi-select.** Cmd-click, shift-click and shift-arrow across the list as
  sorted and filtered; a band shows what is marked. Delete several skills under
  one confirmation, and link or unlink several to an agent in one operation.
- **9. A repo full of skills** is enumerated and offered with a checkbox each,
  before any archive is downloaded.
- **10. Update all**, skipping the skills that need the per-skill confirmation
  and naming them rather than quietly doing fewer.
- **11. All agents on or off** in one operation.

## Things the app now shows

- **12. Reach is on every row.**
- **13. A scope for skills with an update.**
- **14. "See what changed"** links to the upstream comparison from the
  install-time commit, which is now recorded. Skills installed before that get
  a link to the directory upstream, labelled honestly as a listing.
- **15/16.** The update dialog names where the old copy goes. A skill reached
  through the shared directory shows as on, with the control that governs it
  named, instead of showing Off while the header says otherwise.
- **17.** A zero use count says nothing was recorded, not that nothing was used.

## Rough edges

- **18.** Down and Enter carry focus from the search field into the list; a
  mouse click leaves the list focused.
- **19.** Delete, Reveal, Update, Discover, Install and Check for updates have
  menu items and shortcuts. Cmd-W closes a file tab when one is open.
- **20.** The New skill dialog validates as you type and survives a refusal.
- **21.** An empty search offers to search skills.sh for what you typed.
- **22.** A skill can be imported from any folder on disk.
- **23.** Name and description are form fields; renaming moves the directory
  and repoints every link.
- **24.** The window's size and position, the selected skill, the scope, the
  pane split and both collapsed groups are remembered across launches. A
  remembered window that no longer lands on an attached display opens centred
  instead.
- **25.** The accent colour reaches the selected row and the switches, and
  every macOS accent is fitted to a usable contrast rather than discarded.
  Changing it no longer needs a relaunch.
- **26.** Adopt and Release confirm, naming source and destination.
- **27.** An overridden `SKILLBASE_HOME` is in the window title.
- **28.** Uninstalled agents are named; a permission failure says what to do.
- **29.** Duplicate rows can be revealed, and the differing-file list expands.
- **30.** The install-record cache, the usage cache and the staging sweep all
  report their failures, and a frontmatter that cannot be serialized is refused
  rather than written while reporting success.

## The review

Four agents reviewed the change set against the files they owned, and their
findings were fixed by eight more. What the review caught, in the order it
matters:

- **Save in the unsaved-edits dialog did nothing** when the name had been
  edited. Dialogs are a stack, and the Save handler popped the rename
  confirmation the save had just opened.
- **A failed install could leave the user's existing skill only in the trash**,
  with nothing naming that directory. The destructive half of the operation had
  a cross-device fallback and the constructive half did not.
- **The saved window frame and the display list were in different coordinate
  spaces**, so a second monitor restored the window off-screen. Fixed by
  remembering the display's UUID and treating the frame as screen-relative.
- **`list_width` was saved and never read back.**
- **Deleting a skill left its remote record behind**, and renaming one orphaned
  it. Either way the next skill to take that name inherited a stranger's
  install-time digest and upstream link.
- **The trash was inside `scope_roots`**, so a deleted skill could not be
  imported back — the recovery path this change set advertised was closed.
- **An import did not hold the one-job-at-a-time slot** while it measured the
  folder, then cleared a slot it never took.
- Smaller: a stuck `Checking` state, a stale mirrored preference, a
  non-atomic settings write, `completed()` lying about a rollback, Mark All
  doing nothing unless the list held focus, a tint that could fail the contrast
  it documented, and several tests that asserted less than their names claimed.

## Not done

- **There is no gpui test harness in this crate**, so nothing above that lives
  in the interface can have a regression test. The Save bug was entirely in the
  order of two window calls; no test could have caught it, and none can now.
  Adding `gpui-kit` with `test-support` as a dev-dependency is the way in.
- The app was built, run and captured. Note that a capture cannot currently
  show anything that lands after the first couple of frames — see the note in
  the screenshot script — so the running check is weaker than it looks.
