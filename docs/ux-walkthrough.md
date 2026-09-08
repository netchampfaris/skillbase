# Driving Skillbase as a first-time user

Written 9 September 2026, against the release build now installed at
`~/Applications/Skillbase.app` — that is, the uncommitted working tree, not
HEAD.

## How this was done

The application was driven with real mouse and keyboard events (`CGEventPost`
to the HID event tap) and photographed after each step with `screencapture -l`.
It ran against a throwaway `SKILLBASE_HOME` holding `.claude`, `.codex`,
`.cursor` and `.aider-desk` but no skills at all — a machine where somebody has
agents installed and has just opened Skillbase for the first time.

This takes keyboard focus, which the note in `script/screenshot.sh` exists to
avoid. That was the point of the exercise, so the usual rule was set aside for
it. The real `~/.agents/skills` was never touched: it held 62 skills before and
after.

The journeys walked: first launch, Discover, search, install one, install the
same one twice, install a second, install from GitHub by URL and by
`owner/repo`, delete one, mark several and delete them, link a skill to an
agent, rename a skill, search the library, and Settings.

## Two crashes

Both are aborts — the process dies, the window vanishes, nothing is saved.
Both are in code that exists only in the working tree; HEAD does not have
them.

### 1. "New skill" kills the application

Launch, click **New skill** in the sidebar. One click, nothing else. The
process panics and aborts:

```
cannot read skillbase::app::Skillbase while it is already being updated
gpui-pre-0.3.3/src/app/entity_map.rs:164
```

Reproduced four times from a clean launch. The same click on a HEAD build opens
the dialog normally, so this is a regression.

The cause is at `src/ui/list.rs:1878`:

```rust
let scan = this
    .upgrade()
    .and_then(|entity| entity.read(cx).scan().cloned());
```

`open_new_skill_dialog` is called with `&mut Context<Self>`, so the `Skillbase`
entity is mid-update. `window.open_dialog` runs its builder immediately, and
the builder reads the entity that is being updated. The comment above it is
right that the builder runs on every frame; it is the first of those frames
that panics.

This is one of the four commands the empty state names, so it is the first
thing a new user is invited to try.

### 2. Installing a repository that holds several skills kills it too

**Install from GitHub** → `anthropics/skills` → **Install**. Same panic, same
abort. `anthropics/skills/skills/docx` — a path to one skill — installs fine, so
it is the chooser specifically.

Same cause, at `src/ui/discover.rs:679`:

```rust
let choices: Vec<(SharedString, bool)> = this
    .upgrade()
    .map(|entity| entity.read(cx).install_choices...)
```

The dialog's own help text advertises this path: "A repository that holds
several skills asks which of them to install."

These are the only two of the eight `open_dialog` builders that read the root
entity. The other six take `_` for the context and are fine.

## Friction, worst first

### 3. Installing a skill does not make Claude Code see it, and nothing says so

This is the whole point of the application and it is the thing a first-time
user gets wrong.

Install writes to `~/.agents/skills` and creates no links. Claude Code has
`reads_shared: false`, so it does not read that directory. After installing
two skills, the sidebar still read **Claude Code 0**. The skill becomes visible
only after finding the collapsed **Visible to** disclosure at the bottom of the
detail pane — below a file tree that is 30 rows long for `docx` — and flipping
a switch.

Nothing at install time mentions this. The Discover header says "Installing one
writes it into ~/.agents/skills" and the toast says "Wrote 1 file to
~/.agents/skills/react-pdf". Both are true and neither answers "can my agent
use it now?".

### 4. Three numbers on one screen disagree about which agents exist

On the detail pane for a skill, two lines apart:

- **Visible to** — Shared, Codex, Cursor, +10
- "12 agents are not installed on this machine, so they have no rows here:
  Codex, Cursor, Gemini CLI, …"

Codex and Cursor are in both lists. The sidebar says **Agents 2 of 14**;
Settings says **3 of 16**. Four counts of the same thing, no two alike.

### 5. Zed is reported as installed on a machine with no Zed

An agent counts as installed when its skills directory exists
(`src/ui/model.rs:991`). Zed's `global_dir` is `GlobalDir::Shared`, so its
directory *is* `~/.agents/skills` — the directory Skillbase creates itself. On
the throwaway home the Agents group read "0 of 14" until the first install, and
"1 of 14 — Zed 2" immediately after. Zed was never on the machine.

### 6. Claude Code is reported as absent when it plainly is not

The same rule the other way. `.claude/` existed; `.claude/skills/` did not, so
Skillbase said "Claude Code is not installed on this machine" and offered no
row and no way to link. Creating an empty `.claude/skills` made it appear.

Anyone who installs Claude Code and opens Skillbase before creating their first
skill will be told their main agent is not there.

### 7. Discover never says a skill is already installed

Install `pdf` from `anthropics/skills`, go back to Discover, and its button
still reads **Install**. Clicking it downloads the whole thing again and only
then opens a conflict dialog offering Replace, Keep both, or Cancel. The dialog
is well written; it is the recovery, not the prevention. **Replace** is the
default button, which for an identical re-download is the destructive choice.

### 8. Enter in the Install-from-GitHub dialog throws away what you typed

Type a repository, press Return. The dialog closes. Nothing installs, no error,
no notification — and the text is gone. Clicking **Install** with the identical
text produces a proper error, so Return is not submitting.

### 9. That dialog does not focus its only field

Open **Install from GitHub** and type. Nothing appears. The field has to be
clicked first. HEAD focuses it on open; the working tree does not.

### 10. A listed skill fails with a developer's error

**pdftk-server** from `github/awesome-copilot` — 10,022 installs, sixth result
for "pdf" — fails after about fourteen seconds with:

> Could not install
> https://codeload.github.com/github/awesome-copilot/tar.gz/f95f1b4c… :
> response body is larger than 67108864 bytes

The skill is small; the repository tarball is not. The message names an
internal limit in bytes and gives no next step. Any skill in a large monorepo
is uninstallable and the catalogue never says so.

### 11. Error notifications never leave

The `pdftk-server` error sat on screen through five subsequent actions and was
still there minutes later, over the top-right of the content area where the
Save button and toolbar live. When a second error arrived it drew over the
first, leaving an unreadable overlap of two messages. A stale error sitting
over a fresh dialog reads as that dialog's result.

### 12. Deleting one skill and deleting several describe themselves differently

- One skill: "Removes the directory it lives in: ~/.agents/skills/react-pdf",
  with a red **Delete**.
- Several: "Directories move to ~/.skillbase/trash. Links are removed."

The second is accurate — the delete does move to trash. The first sounds
permanent and does not mention the links either. The commoner path has the
worse copy. Afterwards the toast names the trash directory with a unix
timestamp suffix, and there is no undo and no view of the trash: recovering
means going to the Finder.

### 13. Marking several skills is undiscoverable

Shift-click a second row and a bar appears with Link, Unlink, Delete and Clear.
It works well. Nothing anywhere suggests it exists — no checkboxes until
something is marked, no hint in the list header, no context menu. A new user
deletes three skills one at a time.

### 14. Installing kicks you out of Discover

Each install switches to the library and selects what was installed. To install
a second thing you go back to Discover; the query is still there, which helps,
but the round trip happens on every single install and there is no way to
install several at once.

### 15. Results carry no descriptions

Searching "pdf" returns 50 results including three called exactly `pdf`, from
`anthropics/skills`, `openai/skills` and `nexu-io/open-design`. Rows show a name,
an owner and an install count. Clicking a row does nothing. There is no way to
read what any of them does before installing one.

### 16. The first-run screen explains nothing and asks for nothing

The empty state is one run-on sentence naming four sidebar commands that are
already visible beside it: "Four commands in the sidebar add one: Discover
searches skills.sh, Install from GitHub downloads one, Install from folder
copies one already on this machine, and New skill starts an empty one." There is
no primary action, "skills.sh" is unexplained, and the right-hand pane
simultaneously says "No skill selected — Pick a skill from the list to read or
edit it" over an empty list.

The vocabulary the whole application runs on — store, managed, shared, adopt,
release, link — is defined only behind an unlabelled (i) at the top of the list
pane, which also explains the *sidebar* groups from above the *list*.

### 17. Smaller things

- The list header keeps showing **All 2** while a search filters it to one row.
- Two rows both truncate to "Use this skill whenever the…", so `pdf` and `docx`
  are indistinguishable in the list.
- A skill's file tree is neither collapsible nor separately scrollable, so for
  `docx` everything below it — Location, Visible to, Source — is roughly a
  screen and a half down.
- Editing only the **Name** field puts a dirty dot on the **SKILL.md** tab as
  well as **Overview**.
- Once linked, an agent row grows a second switch, **Enabled**, next to
  **Linked**. Nothing distinguishes them.
- Downloads show "Downloading <name>" and nothing else — no size, no progress —
  and `pdftk-server` sat there for fourteen seconds before failing.
- The name rule under the field ("Use lowercase letters, digits, and single
  hyphens between them: my-new-skill") appears as you type and is one of the
  best things in the interface.

## Not checked

- **Install from folder** — it opens a native file panel, which this harness
  cannot drive without more work than it was worth.
- **⌘A in the Name field** appeared to do nothing, but synthetic modifier
  events are unreliable enough here that I am not reporting it as a defect.
- Whether crash 2 also affects HEAD. HEAD has no multi-skill chooser at all
  (`grep` finds the strings only in the working tree), so the path is new.
