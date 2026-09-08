# Skillbase UX issues

Twenty-three issues found by driving the application as a first-time user on
9 September 2026. The walkthrough they came from, with screenshots' worth of
detail and the reasoning behind each judgement, is `docs/ux-walkthrough.md`.
This file is the work list.

Everything below was found against the **uncommitted working tree**, built as a
release bundle. Two of the issues are regressions that do not exist at HEAD.

## Before you start

- Nothing in the working tree is committed. `git status` shows 25 modified
  files plus `docs/ux-review.md`, `docs/ux-walkthrough.md` and this file. Do
  not commit the whole tree as one change; the issues below are separable.
- Run the application against a throwaway home so you do not write into the
  user's real store: `SKILLBASE_HOME=/some/scratch/dir`. The real
  `~/.agents/skills` holds 62 skills and must be left alone.
- `script/preview.sh` builds, restarts and screenshots without taking keyboard
  focus. Prefer it. If you need to actually click things, the notes in
  `docs/ux-walkthrough.md` under "How this was done" say what works: synthetic
  `CGEvent`s posted to the HID event tap; AppleScript `click at` does nothing
  to a GPUI window.
- `cargo test` covers `skillbase-core` well and the UI barely at all. Several
  issues below can carry a test; where one is worth writing, it says so.

## Order to work in

1. Issues 1 and 2 first. They are aborts, and issue 1 fires on the first thing
   a new user is invited to click.
2. Then 3 through 8. They are the ones that make a user believe something
   false about their own machine.
3. Then the rest.

---

## Crashes

### 1. Clicking "New skill" kills the application

**Severity:** critical, regression against HEAD.

**Repro:** launch, click **New skill** in the sidebar. One click. The process
panics and aborts:

```
cannot read skillbase::app::Skillbase while it is already being updated
gpui-pre-0.3.3/src/app/entity_map.rs:164
```

Reproduced four times from a clean launch. The same click on a HEAD build opens
the dialog.

**Where:** `src/ui/list.rs:1878`, inside `open_new_skill_dialog`
(`src/ui/list.rs:1858`).

```rust
let scan = this
    .upgrade()
    .and_then(|entity| entity.read(cx).scan().cloned());
```

**Cause:** `open_new_skill_dialog` takes `&mut Context<Self>`, so the
`Skillbase` entity is mid-update when it is called. `window.open_dialog` runs
its builder immediately, and the builder reads the entity that is being
updated.

**Done when:** clicking **New skill** opens the dialog, and the dialog still
knows which names are taken. Read the scan once before `open_dialog` and move
what the builder needs into the closure, rather than reading the entity from
inside it.

### 2. Installing a repository that holds several skills kills it too

**Severity:** critical, regression (the path is new — HEAD has no multi-skill
chooser at all).

**Repro:** **Install from GitHub** → `anthropics/skills` → **Install**. Same
panic, same abort. `anthropics/skills/skills/docx`, a path to a single skill,
installs fine, so it is the chooser specifically.

**Where:** `src/ui/discover.rs:679`, inside `open_location_dialog`
(`src/ui/discover.rs:659`).

```rust
let choices: Vec<(SharedString, bool)> = this
    .upgrade()
    .map(|entity| entity.read(cx).install_choices...)
```

**Cause:** identical to issue 1.

**Done when:** `anthropics/skills` opens the chooser, ticking rows updates the
tick state and the "select all" state, and installing the ticked ones works.

**Note for both:** these are the only two of the eight `open_dialog` builders
that read the root entity. The other six take `_` for the context. The full
list, so you can check nothing else grows the same shape:

```
src/ui/discover.rs:677   <-- issue 2
src/ui/discover.rs:865
src/ui/discover.rs:1031
src/ui/list.rs:956
src/ui/list.rs:1187
src/ui/list.rs:1868      <-- issue 1
src/ui/detail.rs:1781    (uses cx for formatting only, safe)
src/ui/detail.rs:3382
```

A test that opens each dialog would have caught both. Worth adding.

---

## The application tells the user things that are not true

### 3. Installing a skill does not make Claude Code see it, and nothing says so

**Severity:** high. This is the application's purpose and it is the thing a
first-time user gets wrong.

**Repro:** install two skills from Discover. The sidebar still reads **Claude
Code 0**. The skill is visible to Claude Code only after finding the collapsed
**Visible to** disclosure at the bottom of the detail pane — below a file tree
that is 30 rows long for `docx` — and flipping a switch.

**Cause:** install writes to `~/.agents/skills` and creates no links. Claude
Code has `reads_shared: false` (`crates/core/src/registry.rs:187-194`), so it
does not read that directory.

**What the user is told instead:** the Discover header says "Installing one
writes it into ~/.agents/skills"; the toast says "Wrote 1 file to
~/.agents/skills/react-pdf" (`src/ui/mod.rs:360-368`). Both true, neither
answers "can my agent use it now?".

**Done when:** after an install, the user knows whether their agents can see
the skill, and can act on it without hunting. Two candidate shapes, pick one:

- The install notification names the agents that cannot see it yet and offers
  linking, or
- the post-install flow puts the **Visible to** section in front of the user.
  Note `src/app.rs:1163` already sets `open_visibility_after_scan = true`, so
  the intent exists; it is not reaching the user.

Whichever you choose, resolve it against issue 14 — the user has asked that an
install not leave the Discover page at all.

### 4. Four numbers on one screen disagree about which agents exist

**Severity:** high.

**Repro:** open a skill's detail pane. Two lines apart:

- **Visible to** — Shared, Codex, Cursor, +10
- "12 agents are not installed on this machine, so they have no rows here:
  Codex, Cursor, Gemini CLI, …"

Codex and Cursor are in both lists. Meanwhile the sidebar says **Agents 2 of
14** and Settings says **3 of 16**.

**Where:**

- the split into shown and absent: `src/ui/detail.rs:2217-2227`
- the sentence: `src/ui/detail.rs:3908` (`absent_sentence`)
- sidebar count: `src/ui/sidebar.rs:183-189`
- Settings count: `src/ui/settings.rs:112-118` — this one counts *directories*,
  not agents, which is why its denominator is 16

**Cause:** the detail pane shows an agent when `installed.contains(agent) ||
skill.linked_to(agent.id)`, and lists it as absent when neither holds — but the
two lists are built from different inputs and an agent can satisfy the first
test through `linked_to` while still counting as not installed. The four
counters use three different denominators.

**Done when:** the detail pane never names an agent in both lists, and the
sidebar, Settings and detail pane agree — or, where they are counting genuinely
different things, each says what it is counting.

### 5. Zed is reported as installed on a machine with no Zed

**Severity:** high.

**Repro:** on a home with no agents at all, the Agents group reads "0 of 14".
Install one skill. It reads "1 of 14 — Zed 2". Zed was never on the machine.

**Where:** `src/ui/model.rs:988-992`.

```rust
.filter(|agent| roots.agent_dir(agent).is_dir())
```

**Cause:** an agent counts as installed when its skills directory exists. Zed's
`global_dir` is `GlobalDir::Shared` (`crates/core/src/registry.rs:259-268`), so
its directory *is* `~/.agents/skills` — the directory Skillbase creates itself.

**Done when:** an agent whose directory is the shared store is not inferred to
be installed from the existence of that store. Either exclude shared-dir agents
from this test or find another signal for them.

### 6. Claude Code is reported as absent when it plainly is not

**Severity:** high.

**Repro:** a home with `.claude/` but no `.claude/skills/`. Skillbase says
"Claude Code is not installed on this machine", offers no row and no way to
link. Creating an empty `.claude/skills` makes it appear.

**Where:** the same filter, `src/ui/model.rs:991`.

**Cause:** the directory tested is `.claude/skills`, which does not exist until
somebody creates their first skill. Anyone who installs Claude Code and opens
Skillbase before that is told their main agent is not there.

**Done when:** an agent installed but without a skills directory yet is shown,
with linking available — creating the directory is Skillbase's job. Test
against the agent's own root (`~/.claude`) rather than its skills subdirectory,
or against whatever each `AgentDef` can offer as a presence signal.

Issues 5 and 6 are the same rule failing in both directions; fix them together.

---

## Discover

### 7. Installing from Discover throws you out of Discover

**Severity:** high. Raised directly by the user: *"in discover when i install a
skill, it changes to the skill detail page, which breaks the flow if i want to
download multiple skills from discover page. it should stay in discover page."*

**Repro:** search, install one result. The work area switches to the library
with the new skill selected. To install a second thing you go back to Discover.
The query survives the round trip, but the trip happens on every single
install, and there is no way to install several at once.

**Where:** `src/app.rs:1152-1165`.

```rust
pub(crate) fn installed(&mut self, name: SharedString, ...) {
    self.work_area = WorkArea::Skills;
    self.scope = Scope::Library(Library::All);
    self.check_after_scan = true;
    self.open_visibility_after_scan = true;
    self.rescan(Some(name), window, cx);
}
```

Called from `src/ui/discover.rs:1267`, `:1487` and `:1492`.

**Done when:** installing from Discover leaves the user on Discover, with the
results and query intact and the installed row showing its new state (see issue
8). The rescan still has to run — it is what makes the library correct — but it
must not move the work area.

Two things are entangled here and both need an answer:

- Installing from the **Install from GitHub** dialog, which is not a browsing
  flow, may reasonably still land on the skill. Decide per entry point rather
  than changing `installed` for everyone.
- Issue 3 wants the user to see **Visible to** after an install. If Discover no
  longer navigates, that has to happen some other way — in the notification, or
  in the Discover row itself.

### 8. Discover never says a skill is already installed

**Severity:** high.

**Repro:** install `pdf` from `anthropics/skills`. Go back to Discover. Its
button still reads **Install**. Click it: the whole thing downloads again, and
only then does a conflict dialog offer Replace, Keep both, Cancel. **Replace**
is the default button, which for an identical re-download is the destructive
choice.

**Where:** the row renderer is `src/ui/discover.rs:516` (`fn result_row`). The
conflict dialog is `src/ui/discover.rs:865` and it is well written — it is the
recovery, not the prevention.

**Done when:** a result already in the store says so before it is clicked, and
the click does something other than re-download it. Also reconsider **Replace**
as the default button.

### 9. Enter in the Install-from-GitHub dialog throws away what you typed

**Severity:** medium.

**Repro:** open **Install from GitHub**, type a repository, press Return. The
dialog closes. Nothing installs, no error, no notification, and the text is
gone. Clicking **Install** with the identical text produces a proper error, so
Return is not reaching submit.

**Where:** `src/ui/discover.rs:1031` (the dialog), `src/ui/discover.rs:1092`
(`fn install_from_spec`).

**Done when:** Return in that field runs the same thing the **Install** button
runs.

### 10. That dialog does not focus its only field

**Severity:** medium, regression — HEAD focuses it on open.

**Repro:** open **Install from GitHub** and type. Nothing appears. The field
has to be clicked first.

**Where:** `src/ui/discover.rs:1031`.

**Done when:** the field has focus when the dialog opens. Compare against HEAD
to see what was dropped.

### 11. Results carry no descriptions

**Severity:** medium.

**Repro:** search "pdf". Fifty results, three of them called exactly `pdf`,
from `anthropics/skills`, `openai/skills` and `nexu-io/open-design`. Rows show
a name, an owner and an install count. Clicking a row does nothing. There is no
way to read what any of them does before installing it.

**Where:** `src/ui/discover.rs:516` (`fn result_row`). Check whether
`SearchHit` already carries a description from skills.sh; if it does not, this
needs the registry side first.

**Done when:** a user can tell two identically-named results apart without
installing both.

### 12. A listed skill fails with a developer's error

**Severity:** medium.

**Repro:** search "pdf", install **pdftk-server** from `github/awesome-copilot`
— 10,022 installs, sixth result. After about fourteen seconds:

> Could not install
> https://codeload.github.com/github/awesome-copilot/tar.gz/f95f1b4c… :
> response body is larger than 67108864 bytes

**Where:** `crates/core/src/http.rs:25` (`MAX_BODY_BYTES = 64 * 1024 * 1024`),
error variant at `:48-53`, enforced at `:222-225`.

**Cause:** the skill is small; the repository tarball is not. Any skill in a
large monorepo is uninstallable, and the catalogue never says so.

**Done when:** the message says what happened in the user's terms — the
repository is too large to download whole — and, if there is one, offers the
next step. Fetching a subtree rather than the whole tarball would remove the
failure entirely and is the better fix if it is available; if it is not, say so
in the message and raise the limit only with a reason.

---

## Notifications, deleting, and finding things

### 13. Error notifications never leave, and they overlap

**Severity:** medium.

**Repro:** trigger the `pdftk-server` failure. The error sits on screen through
five subsequent actions and is still there minutes later, over the top-right of
the content area where the Save button and toolbar live. Trigger a second
error: it draws over the first, leaving two messages illegibly on top of each
other.

**Where:** every `.autohide(false)` — `src/ui/mod.rs:202, 306, 325, 383, 395`
and `src/app.rs:695, 1223, 1300, 1539, 1543`.

**Cause:** the reasoning in the comments is sound (nothing else records that
the install failed, so the sentence must not vanish). The failure is that it
never becomes dismissable in practice and that two of them collide.

**Done when:** two errors stack rather than overlap, and a persistent error can
be cleared without hunting. A stale error sitting over a fresh dialog reads as
that dialog's result, which is the real damage.

### 14. Deleting one skill and deleting several describe themselves differently

**Severity:** medium.

**Repro:**

- One skill: "Removes the directory it lives in:
  ~/.agents/skills/react-pdf", with a red **Delete**.
- Several: "Directories move to ~/.skillbase/trash. Links are removed."

The second is accurate — the delete does move to trash. The first sounds
permanent and does not mention links.

**Where:** single at `src/ui/detail.rs:1441-1495` (the "the directory it lives
in" phrasing is `:1479`, the summary `:1494`); bulk at `src/ui/list.rs:1469`.

**Done when:** both dialogs say the same true thing: the directory moves to
trash, and links are removed. The commoner path currently has the worse copy.

### 15. There is no undo and no view of the trash

**Severity:** medium.

**Repro:** delete a skill. The toast names the trash directory with a Unix
timestamp suffix. Recovering means going to the Finder.

**Where:** the trash is `~/.skillbase/trash`.

**Done when:** a deleted skill can be restored from inside Skillbase, or at
minimum the trash can be opened from it. An undo action on the notification is
the cheaper half and worth doing on its own.

### 16. Marking several skills is undiscoverable

**Severity:** medium.

**Repro:** shift-click a second row. A bar appears with Link, Unlink, Delete
and Clear. It works well. Nothing anywhere suggests it exists — no checkboxes
until something is marked, no hint in the list header, no context menu. A new
user deletes three skills one at a time.

**Where:** the marked-selection bar is around `src/ui/list.rs:1075`; the bulk
delete dialog is `src/ui/list.rs:1373`.

**Done when:** a user who has never used the application can find multi-select.
A context menu on a row is the smallest change that would do it.

### 17. The first-run screen explains nothing and asks for nothing

**Severity:** medium.

**Repro:** launch with no skills. The empty state is one run-on sentence naming
four sidebar commands that are already visible beside it:

> Four commands in the sidebar add one: Discover searches skills.sh, Install
> from GitHub downloads one, Install from folder copies one already on this
> machine, and New skill starts an empty one.

There is no primary action, "skills.sh" is unexplained, and the right-hand pane
simultaneously says "No skill selected — Pick a skill from the list to read or
edit it" over an empty list.

**Where:** `src/ui/list.rs:405-406`.

**Done when:** first launch has one obvious thing to do — a primary button onto
Discover — and does not narrate the sidebar back to the user.

Related: the vocabulary the whole application runs on — store, managed, shared,
adopt, release, link — is defined only behind an unlabelled (i) at the top of
the list pane (`src/ui/list.rs:495-500`), which also explains the *sidebar*
groups from above the *list*. Move that explanation to where each word is first
used, or label the button.

---

## Smaller things

### 18. The list header count ignores the search

**Severity:** low.

The header keeps showing **All 2** while a search has filtered the list to one
row. `src/ui/list.rs:460`: `scan.count(self.scope, ...)` does not see
`self.query`.

**Done when:** the number matches the rows on screen, or says "1 of 2".

### 19. Two skills are indistinguishable in the list

**Severity:** low.

`pdf` and `docx` both truncate to "Use this skill whenever the…". The row's
description column is `src/ui/list.rs:1779-1808`, the fallback text
`src/ui/list.rs:1573-1576`.

**Done when:** rows whose descriptions share a prefix can still be told apart —
a tooltip on the truncated text is the cheapest fix.

### 20. The file tree is neither collapsible nor separately scrollable

**Severity:** low.

For `docx` the tree is 30 rows, so everything below it — Location, Visible to,
Source — is roughly a screen and a half down. This is what makes issue 3 as bad
as it is.

**Done when:** the tree collapses or scrolls in its own pane, and **Visible
to** is reachable without a long scroll.

### 21. Editing only the Name field marks the SKILL.md tab dirty

**Severity:** low.

Both the Overview and the **SKILL.md** tab grow a dirty dot when only **Name**
has changed. This is deliberate — `src/ui/detail.rs:3317-3329`, the Overview
edits `SKILL.md`'s frontmatter, so it is dirty when that is — but the user has
not touched the file and the dot says they have.

**Done when:** the user is not told they have unsaved changes in a file they
have not opened. Showing the dot only on the tab being edited, with the save
still writing `SKILL.md`, is probably enough.

### 22. Enabled and Linked are not distinguished

**Severity:** low.

Once linked, an agent row grows a second switch, **Enabled**
(`src/ui/detail.rs:2584`), next to **Linked** (`src/ui/detail.rs:2618`).
Nothing on screen says what the difference is. The comment at
`src/ui/detail.rs:2601-2604` shows the naming was thought about; the thinking
did not reach the user.

**Done when:** each switch says what turning it off does — a tooltip would do.

### 23. Downloads show no progress

**Severity:** low.

Downloads show "Downloading <name>" and nothing else — no size, no percentage.
`pdftk-server` sat there for fourteen seconds before failing (issue 12).

**Where:** `src/ui/discover.rs:1338` and `Installing`
(`src/ui/discover.rs:94`).

**Done when:** a download longer than a second or two shows that it is
progressing.

---

## One thing to keep

The name rule under the New skill field — "Use lowercase letters, digits, and
single hyphens between them: my-new-skill" — appears as you type and is one of
the best things in the interface. Do not lose it while fixing issue 1.

## Not checked

- **Install from folder.** It opens a native file panel, which the harness
  could not drive.
- **Cmd-A in the Name field** appeared to do nothing, but synthetic modifier
  events were unreliable enough that it is not reported as a defect. Check it
  by hand.
- **Whether issue 2 also affects HEAD.** HEAD has no multi-skill chooser at all
  — grep finds the strings only in the working tree — so the path is new, but
  this was not confirmed by running HEAD.
