//! The detail pane: what the selected skill says it is, the agents it is
//! visible to, the whole `SKILL.md` in a code editor, and the actions that
//! change any of it on disk.
//!
//! The name and the description are frontmatter, so the `SKILL.md` tab is
//! where they are edited. The Overview reads them out and nothing more. The
//! one exception is renaming, which moves a directory rather than editing a
//! line, and has its own action and its own confirmation.
//!
//! Every write goes through `skillbase-core` against the one [`Roots`] the
//! application resolved at startup, and every write happens on a background
//! task. When the disk changes, this pane emits [`DetailEvent::Changed`] and
//! the root view scans again, so the interface never asserts a state the
//! filesystem does not back up.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::slice;

use gpui_kit::base::Selectable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::{DialogButtonProps, DialogClose, DialogFooter};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::label::Label;
use gpui_kit::component::link::Link;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, ElementId, Entity, EventEmitter,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px, rems,
};
use skillbase_core::{
    AgentDef, Change, ConsolidatePlan, DeletePlan, DisableMode, Duplicate, InstallError,
    InstallOptions, Installer, LocalState, LocationKind, MAX_NAME_LEN, Outcome, PRIVATE_ID,
    Provenance, Registry, RemoteCache, RepoRef, Roots, SKILL_FILE_NAME, Skill, SkillDoc,
    SkillError, SkillLocation, UpdateReport, UpdateStatus, content_digest, is_kebab_case,
    local_state, repo_key,
};

use super::model::{
    Issue, Scan, SkillView, agent_label, ago, display_path, has_disable_state, short_sha,
};
use super::{
    BAND_HEIGHT, PROSE_MAX_WIDTH, agent_icon, cache_failure_notification,
    delete_cache_failure_notification, delete_effect, drag_band, install_skill, push_notice,
    remember_delete_cache_failure, report, report_delete, report_install,
    take_delete_cache_failure,
};

/// How many differing paths a duplicate lists before it starts counting.
const DIFF_PATHS: usize = 6;

/// What runs once the unsaved edits in the active tab have been dealt with.
///
/// Every way out of an edit — another skill, another scope, closing the window,
/// quitting — is one of these, held until the user has said whether to save,
/// discard, or stay.
pub(crate) type Proceed = Box<dyn FnOnce(&mut Window, &mut App) + 'static>;

/// What this pane tells the root view.
pub enum DetailEvent {
    /// The filesystem changed. Scan again, and land on `select` afterwards.
    Changed { select: Option<SharedString> },
    /// The skill was replaced by the copy upstream holds. Scan again, land on
    /// `select`, and ask GitHub again: the tree sha this skill records has
    /// just moved, so the last check's answer for it is no longer true.
    Updated { select: Option<SharedString> },
}

/// Where the `SKILL.md` read has got to.
enum Source {
    Empty,
    Loading,
    Loaded,
    Failed(SharedString),
}

/// Where the comparison of a skill's duplicate directories has got to.
///
/// Comparing eight copies of a skill file by file is filesystem work, so it
/// happens on a background task and the pane renders whichever of these three
/// states it is in.
enum Duplicates {
    /// This skill has no duplicate directories.
    None,
    Comparing,
    Ready(ConsolidatePlan),
}

/// Where the read of the selected skill's origin has got to.
///
/// Two questions, and only the second costs anything: where the skill came
/// from, which its own frontmatter answers, and whether the directory still
/// holds what was installed, which is a hash of every file in it. That is
/// filesystem work, so it happens on a background task.
enum Remote {
    /// The skill records no provenance. Most skills were written by hand and
    /// never came from anywhere, and a Source section on every one of them
    /// would be an empty box repeated down the library.
    None,
    Reading,
    Ready(Origin),
}

/// Where one skill came from, and whether the copy here still matches it.
struct Origin {
    provenance: Provenance,
    local: LocalState,
    /// Seconds since the Unix epoch when this repository was last asked about,
    /// or 0 when it never has been.
    checked_at: i64,
    /// The commit sha the ref pointed at when this copy was installed.
    ///
    /// The commit, not the tree sha the update check compares: the tree sha
    /// answers "has upstream changed?" and nothing else, while this is the one
    /// GitHub's compare view will resolve. Empty for a skill installed before
    /// Skillbase recorded it, and for one another tool installed; see
    /// [`Upstream`] for what is offered instead.
    recorded_commit: String,
}

/// The link out to GitHub under the update sentence.
///
/// Two shapes, and the label goes with the shape rather than being written
/// once and reused: one of them is a comparison and the other is a directory
/// listing, and a link that says "See what changed" over a listing is a link
/// that lies.
#[derive(Clone)]
struct Upstream {
    url: SharedString,
    label: &'static str,
}

/// One entry in a skill directory, flattened depth-first.
///
/// The directory is shown as a tree rather than a row of tags because its files
/// are what the user clicks to edit, and a bare name does not say which
/// directory a file is in.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileNode {
    /// Path relative to the skill directory — `references/api.md`, never
    /// absolute. Both the row's identity and what a tab is keyed on.
    rel: SharedString,
    /// The last segment, which is what the row shows.
    label: SharedString,
    /// How many directories deep, for the row's indent. Top level is 0.
    depth: usize,
    is_dir: bool,
    /// False for the file types no text editor should open.
    editable: bool,
}

/// Extensions that are not text and must not be loaded into an editor.
///
/// A denylist rather than an allowlist: a skill can bundle a file with any
/// extension or none at all, and refusing to open `notes.rst` because it is not
/// on a list would be wrong more often than opening `logo.png` is.
const BINARY_EXTENSIONS: [&str; 21] = [
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "svg", "pdf", "zip", "gz", "tar", "mp3",
    "mp4", "mov", "wav", "woff", "woff2", "ttf", "otf", "wasm",
];

/// How deep the listing goes, and how many entries it will show.
///
/// A skill directory holds a handful of reference files. These caps exist so
/// that one pointed at something enormous — a checked-out repository, a
/// `node_modules` — degrades into a truncated list instead of a hung pane.
const MAX_TREE_DEPTH: usize = 3;
const MAX_TREE_ENTRIES: usize = 200;

/// How many rows the Files listing may hold and still open with the skill.
///
/// Eight is about a third of the Overview at the pane's usual height. A listing
/// that long is read at a glance, so showing it costs the reader nothing; a
/// longer one is a section of its own and waits to be asked for.
const MAX_ROWS_OPEN: usize = 8;

/// Which of the pane's tabs is showing.
///
/// The Overview is a variant rather than index 0 of the file list, so that
/// closing a tab cannot leave the pane with nothing to show. A file is named by
/// its path relative to the skill directory, so closing an earlier tab does not
/// change which tab is active.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Showing {
    Overview,
    File(SharedString),
}

impl Showing {
    /// Whether this tab wears the unsaved-edits dot, given whether `SKILL.md`
    /// has been typed into.
    ///
    /// The dot says "the work you have not saved is in here". Only the
    /// `SKILL.md` tab is written from the pane's own editor; the Overview
    /// reads that file rather than editing it, so it never wears the dot even
    /// though Save on it writes the same file. Every other file tracks its own
    /// edits in its own [`OpenFile`].
    fn dot(&self, source_edited: bool) -> bool {
        match self {
            Showing::Overview => false,
            Showing::File(rel) if rel == SKILL_FILE_NAME => source_edited,
            Showing::File(_) => false,
        }
    }
}

/// What the Rename dialog's field says about itself, worked out on each
/// keystroke and read by the dialog while it draws.
///
/// A plain `Rc`, not the pane and not the field's own entity. The dialog's
/// builder runs from inside the pane's own render, so reading an entity there
/// aborts the process; the subscription on the field writes this instead. Once
/// per keystroke is also the right rate for the half of the answer that costs
/// a `symlink_metadata` call.
#[derive(Default)]
struct RenameCheck {
    /// Why the name typed cannot be used, or `None` when it can.
    problem: Option<SharedString>,
    /// Whether the field holds a name other than the one the skill has. A
    /// rename to the name it already has moves nothing.
    changed: bool,
}

/// A bundled file open in a tab, with its own editor.
///
/// `SKILL.md` is not one of these. It is the file the Overview reads its name
/// and description out of, so it keeps the dedicated `body` editor and the
/// save path that can rename the directory with it. Everything else is a plain
/// text file: read, edited, and written back with nothing in between.
struct OpenFile {
    /// Path relative to the skill directory. Identity for the tab and for the
    /// row that opened it.
    rel: SharedString,
    /// Resolved once when the file was opened, because the selection can move
    /// while the read is in flight.
    path: PathBuf,
    state: Entity<EditorState>,
    dirty: bool,
    /// Set when the file could not be read; the tab then shows the reason
    /// instead of an empty editor.
    error: Option<SharedString>,
    _subscription: Subscription,
}

pub struct DetailPane {
    /// The one home every read and write resolves against.
    roots: Roots,
    scan: Option<Rc<Scan>>,
    skill: Option<SkillView>,
    source: Source,
    /// The frontmatter name and description as they were when the file was
    /// read, so that a form field the user changed can be told apart from one
    /// they never touched.
    loaded_name: SharedString,
    loaded_description: SharedString,
    /// Everything in the skill directory, flattened depth-first. The rows the
    /// user clicks to open a file.
    tree: Vec<FileNode>,
    /// Which tab is showing.
    showing: Showing,
    /// The tab strip's horizontal scroll, so that a tab the user has just
    /// opened can be brought back into view when it lands past the right edge.
    tab_scroll: ScrollHandle,
    /// The bundled files the user has opened, one editor each. `SKILL.md` is
    /// not among them: it has its own editor and its own save path.
    open: Vec<OpenFile>,
    /// The skill's duplicate directories and how each compares to the origin.
    duplicates: Duplicates,
    /// The duplicate directories whose differing-path list has been asked for
    /// in full. Empty by default: a copy that differs in ninety files would
    /// otherwise push everything below it off the pane.
    expanded_diffs: HashSet<PathBuf>,
    /// Whether the "Visible to" section is open. Closed on every selection,
    /// because the pane's usual job is the editor below it.
    visibility_open: bool,
    /// Whether the "Files" listing is open. Decided for each skill as its
    /// directory is read — see [`files_open_by_default`] — and the user's
    /// answer after that.
    files_open: bool,
    /// Bumped on every comparison, so one that lands after the selection moved
    /// on is dropped.
    duplicates_generation: u64,
    /// Where the selected skill came from, and whether it has been edited
    /// since.
    remote: Remote,
    /// Bumped on every read of the above, for the same reason.
    remote_generation: u64,
    /// What the last update check found, pushed in by the root view. The check
    /// covers every skill at once and lands long after any one selection, so
    /// it is handed over rather than read across.
    updates: Option<Rc<UpdateReport>>,
    /// What the discard confirmation should do once it is answered. Held here
    /// rather than captured by the dialog, because the dialog's builder runs on
    /// every frame and a continuation can only run once.
    pending: Option<Proceed>,
    /// The Rename dialog's field. It lives on the pane rather than in the
    /// dialog because the dialog's builder runs on every frame and would
    /// rebuild the field, and what was typed into it, each time.
    name: Entity<InputState>,
    /// What that field says about itself, while the dialog is open. `None`
    /// when it is not.
    rename: Option<Rc<RefCell<RenameCheck>>>,
    body: Entity<EditorState>,
    /// Whether `SKILL.md` has been typed into and not written back.
    source_edited: bool,
    /// True while a write is in flight, so a second click cannot start one.
    busy: bool,
    /// Bumped on every load, so a read that lands after the selection moved on
    /// is dropped rather than applied to the wrong skill.
    generation: u64,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DetailEvent> for DetailPane {}

impl DetailPane {
    pub fn new(roots: Roots, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("my-skill"));
        let body = cx.new(|cx| EditorState::new(window, cx).language("markdown"));

        let subscriptions = vec![
            cx.subscribe(&name, |this, _, event: &InputEvent, cx| {
                this.check_rename(event, cx)
            }),
            cx.subscribe(&body, |this, _, event: &InputEvent, cx| {
                this.mark_body_edited(event, cx)
            }),
        ];

        Self {
            roots,
            scan: None,
            skill: None,
            source: Source::Empty,
            loaded_name: SharedString::default(),
            loaded_description: SharedString::default(),
            tree: Vec::new(),
            showing: Showing::Overview,
            tab_scroll: ScrollHandle::new(),
            open: Vec::new(),
            duplicates: Duplicates::None,
            expanded_diffs: HashSet::new(),
            duplicates_generation: 0,
            remote: Remote::None,
            remote_generation: 0,
            updates: None,
            visibility_open: false,
            files_open: true,
            pending: None,
            name,
            rename: None,
            body,
            source_edited: false,
            busy: false,
            generation: 0,
            _subscriptions: subscriptions,
        }
    }

    /// Adopt a scan and show one of its skills, or nothing.
    pub fn show(
        &mut self,
        scan: Rc<Scan>,
        selected: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let skill = selected.as_ref().and_then(|name| scan.get(name)).cloned();
        let same_file = match (&self.skill, &skill) {
            (Some(before), Some(after)) => {
                before.name == after.name && before.origin == after.origin
            }
            _ => false,
        };
        self.scan = Some(scan);
        self.skill = skill;
        if !same_file {
            // A different skill is a fresh question, so the section closes
            // again. Re-showing the same file — which is what follows every
            // mutation — leaves it as the user left it, or a switch flipped
            // inside it would close the section under the pointer.
            self.visibility_open = false;
            // The expansions belong to the paths of the skill that was
            // showing, and another skill's duplicates are other directories.
            self.expanded_diffs.clear();
            // The open files belong to the skill that was showing. Another
            // skill's directory has its own, so the tabs close with it and the
            // pane comes back to the Overview.
            self.open.clear();
            self.showing = Showing::Overview;
            self.tab_scroll.scroll_to_item(0);
        }

        // Both of these compare the disk against a record of it, not against
        // the editor, so they are refreshed on every show — including the one
        // that follows a mutation, which is exactly when they have changed.
        self.compare_duplicates(window, cx);
        self.read_origin(window, cx);

        // Re-reading the file under an unsaved edit would throw the edit away.
        // A mutation that only moved links leaves the bytes alone, so keep what
        // the editor holds and just refresh the metadata around it.
        if same_file && (self.source_edited || matches!(self.source, Source::Loaded)) {
            cx.notify();
            return;
        }
        self.load_source(window, cx);
    }

    /// Read the selected skill's `SKILL.md` and its bundled files.
    ///
    /// A skill whose frontmatter does not parse has no `doc`, so the file is
    /// always read from disk rather than taken from the scan: that is the only
    /// way a broken skill can be opened for repair.
    fn load_source(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.source_edited = false;

        let Some(skill) = self.skill.clone() else {
            self.source = Source::Empty;
            self.set_tree(Vec::new());
            self.set_body("", window, cx);
            cx.notify();
            return;
        };

        self.source = Source::Loading;
        cx.notify();

        let dir = skill.origin.clone();
        cx.spawn_in(window, async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    let text = fs::read_to_string(dir.join(SKILL_FILE_NAME));
                    (text, list_tree(&dir))
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                if this.generation != generation {
                    return;
                }
                match loaded.0 {
                    Ok(text) => {
                        this.set_tree(loaded.1);
                        this.adopt_source(&text, window, cx);
                    }
                    Err(error) => {
                        this.set_tree(Vec::new());
                        this.source = Source::Failed(error.to_string().into());
                        this.set_body("", window, cx);
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Take the listing of a skill directory, and decide whether it opens.
    ///
    /// The two go together: the listing is the only thing that knows how long
    /// it is, and its length is the whole of the decision.
    fn set_tree(&mut self, tree: Vec<FileNode>) {
        self.files_open = files_open_by_default(tree.len());
        self.tree = tree;
    }

    /// Compare the selected skill's duplicate directories against its origin.
    ///
    /// Reads only, on a background task. A skill with no duplicates costs
    /// nothing: the comparison is not started at all.
    fn compare_duplicates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.duplicates_generation += 1;
        let generation = self.duplicates_generation;

        let Some(skill) = self
            .skill
            .clone()
            .filter(|skill| !skill.conflicts.is_empty())
        else {
            self.duplicates = Duplicates::None;
            return;
        };

        self.duplicates = Duplicates::Comparing;
        let discovered = skill.as_discovered();
        let roots = self.roots.clone();

        cx.spawn_in(window, async move |this, cx| {
            let plan = cx
                .background_spawn(
                    async move { Installer::new(roots).plan_consolidate(&discovered) },
                )
                .await;
            this.update_in(cx, |this, _, cx| {
                if this.duplicates_generation != generation {
                    return;
                }
                this.duplicates = Duplicates::Ready(plan);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Adopt whatever the last update check found.
    ///
    /// Pushed in by the root view rather than read across, because the check
    /// covers every installed skill at once and lands long after any one
    /// selection has been made.
    pub fn set_updates(&mut self, updates: Option<Rc<UpdateReport>>, cx: &mut Context<Self>) {
        self.updates = updates;
        cx.notify();
    }

    /// Read where the selected skill came from, and whether it still holds the
    /// bytes that were installed.
    ///
    /// The second question is a content digest of the whole directory compared
    /// against the one recorded at install, so it is filesystem work and runs
    /// on a background task. A skill with no provenance costs nothing: the read
    /// is not started at all.
    fn read_origin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.remote_generation += 1;
        let generation = self.remote_generation;

        let Some((skill, provenance)) = self
            .skill
            .clone()
            .and_then(|skill| skill.provenance.clone().map(|p| (skill, p)))
        else {
            self.remote = Remote::None;
            return;
        };

        self.remote = Remote::Reading;
        let roots = self.roots.clone();
        let name = skill.name.to_string();
        let dir = skill.origin.clone();

        cx.spawn_in(window, async move |this, cx| {
            let origin = cx
                .background_spawn(async move {
                    let cache = RemoteCache::read(&roots);
                    let checked_at = repo_ref_of(&provenance)
                        .and_then(|repo| cache.repo(&repo_key(&repo)).map(|r| r.checked_at))
                        .unwrap_or(0);
                    let recorded_commit = cache
                        .skill(&name)
                        .map(|record| record.commit_sha.clone())
                        .unwrap_or_default();
                    Origin {
                        local: local_state(&cache, &name, &dir),
                        provenance,
                        checked_at,
                        recorded_commit,
                    }
                })
                .await;
            this.update_in(cx, |this, _, cx| {
                if this.remote_generation != generation {
                    return;
                }
                this.remote = Remote::Ready(origin);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Where to look at upstream, when there is a page worth opening.
    ///
    /// `None` when there is nothing upstream worth opening: a check that found
    /// nothing new, or a source that is not a GitHub repository. The URL
    /// itself is [`upstream_for`].
    fn upstream_link(&self) -> Option<Upstream> {
        let Remote::Ready(origin) = &self.remote else {
            return None;
        };
        if !matches!(
            self.update_status()?,
            UpdateStatus::UpdateAvailable { .. } | UpdateStatus::NoBaseline { .. }
        ) {
            return None;
        }
        Some(upstream_for(
            &repo_ref_of(&origin.provenance)?.slug(),
            &origin.provenance.reference,
            &origin.provenance.path,
            &origin.recorded_commit,
        ))
    }

    /// What the last check said about the selected skill.
    fn update_status(&self) -> Option<&UpdateStatus> {
        let skill = self.skill.as_ref()?;
        self.updates.as_ref()?.status(&skill.name)
    }

    /// True when there is something upstream worth taking.
    ///
    /// `NoBaseline` counts. It means whatever installed this skill recorded no
    /// sha, so Skillbase cannot say the copy here is current; downloading
    /// upstream again is the only way to make it so.
    fn update_available(&self) -> bool {
        matches!(
            self.update_status(),
            Some(UpdateStatus::UpdateAvailable { .. } | UpdateStatus::NoBaseline { .. })
        )
    }

    /// True when an update would land on this skill's own directory.
    ///
    /// A download always writes to `~/.agents/skills/<name>`. For a skill whose
    /// origin is somewhere else — a hidden skill, or one another tool owns —
    /// that is a second directory beside the first rather than a replacement,
    /// so the Update button is not offered and the Source section says why.
    fn updates_in_place(&self, skill: &SkillView) -> bool {
        skill.origin == self.roots.store_dir().join(skill.name.as_ref())
    }

    /// True when the header offers Update, and so when the menu item does
    /// anything. The two read the same conditions, so the menu never promises
    /// an update the pane is not offering.
    fn can_update(&self) -> bool {
        self.skill
            .as_ref()
            .is_some_and(|skill| self.update_available() && self.updates_in_place(skill))
    }

    /// Open the "Visible to" section.
    ///
    /// Called after an install: the skill is on disk but no agent can see it
    /// yet, and which agents get it is the next thing the user came here to
    /// decide.
    pub(crate) fn open_visibility(&mut self, cx: &mut Context<Self>) {
        if !self.visibility_open {
            self.visibility_open = true;
            cx.notify();
        }
    }

    /// Accept, or take back, losing one divergent duplicate's edits.
    ///
    /// Nothing happens on disk here. It only changes what the confirmation
    /// will offer, and the confirmation still has to be accepted.
    fn set_forced(&mut self, path: &Path, on: bool, cx: &mut Context<Self>) {
        let Duplicates::Ready(plan) = &mut self.duplicates else {
            return;
        };
        if on {
            plan.force(path);
        } else {
            plan.unforce(path);
        }
        cx.notify();
    }

    /// Put the file into the editor, and its frontmatter into what the
    /// Overview reads out.
    fn adopt_source(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let parsed = SkillDoc::parse(text).ok();
        let name = parsed
            .as_ref()
            .and_then(|doc| doc.frontmatter.name().map(str::to_string))
            .unwrap_or_default();
        let description = parsed
            .as_ref()
            .and_then(|doc| doc.frontmatter.description().map(str::to_string))
            .unwrap_or_default();

        self.loaded_name = name.into();
        self.loaded_description = description.into();
        self.source = Source::Loaded;
        self.source_edited = false;
        self.set_body(text, window, cx);
        cx.notify();
    }

    /// Put `text` into the `SKILL.md` editor.
    fn set_body(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        // `set_value` does not emit a change event, so loading never marks the
        // pane dirty.
        self.body.update(cx, |state, cx| {
            state.set_value(text.to_string(), window, cx)
        });
    }

    /// The `SKILL.md` editor was typed in.
    fn mark_body_edited(&mut self, event: &InputEvent, cx: &mut Context<Self>) {
        if matches!(event, InputEvent::Change) && !self.source_edited {
            self.source_edited = true;
            cx.notify();
        }
    }

    /// Work out what the Rename dialog's field now holds, for the dialog to
    /// draw on this frame and the ones after it.
    ///
    /// Once per keystroke rather than once per frame, because half the answer
    /// is a `symlink_metadata` call: [`rename_problem`] asks the disk whether
    /// the destination is occupied, and a dialog that asked while it drew
    /// would ask on every frame the user was typing in.
    fn check_rename(&mut self, event: &InputEvent, cx: &mut Context<Self>) {
        if !matches!(event, InputEvent::Change) {
            return;
        }
        let Some(check) = self.rename.clone() else {
            return;
        };
        let typed = self.name.read(cx).value();
        let typed = typed.trim();
        let Some(skill) = self.skill.as_ref() else {
            return;
        };
        // Against the name the skill has, not the one its frontmatter gives
        // itself. The two can differ, and it is the directory that moves, so
        // this is the same comparison [`rename_problem`] makes when it decides
        // a rename has nothing to do.
        *check.borrow_mut() = RenameCheck {
            problem: rename_problem(typed, skill, self.scan.as_deref(), &self.roots),
            changed: typed != skill.name.as_ref(),
        };
        cx.notify();
    }

    // ---------------------------------------------------------------- saving

    /// Write the edits back through `skillbase-core`.
    ///
    /// **The editor holds the document.** Its text is parsed and becomes the
    /// file, so a save writes exactly what the `SKILL.md` tab shows. The
    /// frontmatter is edited there like everything else; nothing on the
    /// Overview writes into it.
    ///
    /// A file that does not parse cannot be saved; the parse error comes back
    /// as a notification and nothing is written.
    ///
    /// `then` runs once the write has landed. The write is a background task,
    /// so anything waiting on it — the quit the user chose Save from, the skill
    /// they were switching to — has to be carried into its completion rather
    /// than run beside it. A failed write runs nothing: the notification says
    /// why, and the edits are still there.
    fn save_then(&mut self, then: Option<Proceed>, window: &mut Window, cx: &mut Context<Self>) {
        if self.skill.is_none() || self.busy {
            return;
        }
        self.write_source(None, then, window, cx);
    }

    /// Ask for a new name, and rename the skill if one is given.
    ///
    /// A rename is not an edit to a field. `name:` in the frontmatter is only
    /// half of a skill's name — the directory it lives in is the other half —
    /// so editing that line in the `SKILL.md` tab leaves the folder behind
    /// under the old name. This is the action that moves both, and it is why
    /// the name is not a form field on the Overview.
    pub(crate) fn open_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(skill) = self.skill.clone() else {
            return;
        };
        // The name it has now, so the field opens on something the reader
        // recognises and a rename is an edit to it rather than a retyping.
        let current = skill.name.clone();
        self.name.update(cx, |state, cx| {
            state.set_value(current.to_string(), window, cx)
        });
        // Fresh for each dialog, so nothing the last one was told is still
        // being drawn. Nothing has been typed yet, so there is no problem to
        // report and nothing to rename to.
        let check = Rc::new(RefCell::new(RenameCheck::default()));
        self.rename = Some(check.clone());

        let field = self.name.clone();
        let this = cx.entity().downgrade();

        // Nothing in this builder may read an entity: it runs from inside the
        // pane's own render, and reading one there aborts the process. What
        // the field says about itself comes from `check`, which the
        // subscription on the field writes once per keystroke.
        window.open_dialog(cx, move |dialog, _, _| {
            let field = field.clone();
            let (ok, cancel) = (this.clone(), this.clone());
            // Read out and the borrow dropped, rather than held for the rest
            // of the builder: what is drawn is a copy of what the last
            // keystroke worked out.
            let (problem, changed) = {
                let check = check.borrow();
                (check.problem.clone(), check.changed)
            };
            let blocked = !changed || problem.is_some();

            dialog
                .title(format!("Rename {current}"))
                .width(px(460.))
                .content(move |content, _, cx| {
                    content.child(
                        v_flex()
                            .p_4()
                            .gap_2()
                            .child(Label::new("Name"))
                            .child(Input::new(&field).small())
                            .child(
                                // One line, under the field it is about: what
                                // a name has to look like and what changing it
                                // moves, until it is a name that cannot be
                                // used, and then why.
                                div()
                                    .text_xs()
                                    .text_color(match &problem {
                                        Some(_) => cx.theme().danger,
                                        None => cx.theme().muted_foreground,
                                    })
                                    .child(problem.clone().unwrap_or_else(|| {
                                        "kebab-case, and the directory's name as well: renaming \
                                         moves the folder on disk and rewrites the frontmatter \
                                         block, so comments in it are dropped."
                                            .into()
                                    })),
                            ),
                    )
                })
                .footer(
                    DialogFooter::new()
                        .p_4()
                        .child(
                            Button::new("cancel-rename")
                                .outline()
                                .label("Cancel")
                                .on_click(move |_, window, cx| {
                                    cancel.update(cx, |this, _| this.rename = None).ok();
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("confirm-rename")
                                .primary()
                                .label("Rename…")
                                // Off until the field holds a name that is
                                // both different and usable, with the reason
                                // showing under it.
                                .disabled(blocked)
                                .on_click(move |_, window, cx| {
                                    let name = ok
                                        .update(cx, |this, cx| this.accept_rename(cx))
                                        .ok()
                                        .flatten();
                                    // A name the disk has taken since the last
                                    // keystroke keeps the dialog, and the line
                                    // under the field now says why.
                                    let Some(name) = name else {
                                        return;
                                    };
                                    // This dialog goes first: `close_dialog`
                                    // pops whichever is on top, and the
                                    // confirmation below would be the one
                                    // popped.
                                    window.close_dialog(cx);
                                    ok.update(cx, |this, cx| {
                                        this.confirm_rename(name, None, window, cx)
                                    })
                                    .ok();
                                }),
                        ),
                )
        });

        // After `open_dialog`, which focuses a handle of its own. The dialog
        // has one field and nothing else to type into.
        self.name.update(cx, |state, cx| state.focus(window, cx));
    }

    /// The name typed into the Rename dialog, once the disk has been asked
    /// again about it.
    ///
    /// `None` when it cannot be used, and the reason is written back into the
    /// dialog's own check so the line under the field says so. A directory can
    /// appear between the last keystroke and the click, and this is the ask
    /// that happens once per rename rather than once per frame.
    fn accept_rename(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let typed = self.name.read(cx).value();
        let typed = typed.trim().to_string();
        let skill = self.skill.as_ref()?;
        // Nothing to move. The button that sends this is off for a name that
        // has not changed, so this is the backstop rather than the path.
        if typed == skill.name.as_ref() {
            return None;
        }
        let problem = rename_problem(&typed, skill, self.scan.as_deref(), &self.roots);
        if let Some(problem) = problem {
            if let Some(check) = &self.rename {
                check.borrow_mut().problem = Some(problem);
            }
            cx.notify();
            return None;
        }
        self.rename = None;
        Some(typed)
    }

    /// Confirm what renaming moves, and where to.
    ///
    /// The name is the directory's name as well as the frontmatter's, so the
    /// rename moves a real directory. It gets the same confirmation shape as
    /// adopting and releasing: name the source, name the destination, and say
    /// what follows the skill.
    fn confirm_rename(
        &mut self,
        new_name: String,
        then: Option<Proceed>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(skill) = self.skill.clone() else {
            return;
        };
        // Held on the pane rather than captured by the dialog, because the
        // dialog's builder runs on every frame and a continuation can only run
        // once. Both buttons deal with it, so a cancelled rename leaves nothing
        // behind for the next one to pick up.
        self.pending = then;

        let old = skill.name.clone();
        let new_name = SharedString::from(new_name);
        let from = display_path(&skill.origin, &self.roots);
        let to = display_path(&skill.origin.with_file_name(new_name.as_ref()), &self.roots);
        // The links are repointed by the move, so an agent holding one keeps
        // the skill. That is the question a move on disk raises, and only the
        // count answers it.
        let links = skill
            .locations
            .iter()
            .filter(|location| matches!(location.kind, LocationKind::Symlink { .. }))
            .count();
        let this = cx.entity().downgrade();

        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (from, to) = (from.clone(), to.clone());
            let new_name = new_name.clone();
            let (ok, cancel) = (this.clone(), this.clone());

            let description = v_flex()
                .gap_3()
                .text_sm()
                .child(div().child(format!(
                    "Moves {from} to {to}, and writes {new_name} into the frontmatter."
                )))
                .when(links > 0, |this| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "{links} symlink{} point at the directory and move with it, so \
                                 the agents holding one keep the skill.",
                                if links == 1 { "" } else { "s" }
                            )),
                    )
                });

            alert
                .title(format!("Rename {old} to {new_name}?"))
                .description(description)
                .width(px(520.))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Rename")
                        .cancel_text("Cancel")
                        .show_cancel(true),
                )
                .on_cancel(move |_, _, cx| {
                    cancel.update(cx, |this, _| this.pending = None).ok();
                    true
                })
                .on_ok(move |_, window, cx| {
                    let new_name = new_name.clone();
                    ok.update(cx, |this, cx| {
                        let then = this.pending.take();
                        this.write_source(Some(new_name.to_string()), then, window, cx);
                    })
                    .ok();
                    true
                })
        });
    }

    /// Write `SKILL.md`, moving the directory first when the skill is being
    /// renamed.
    ///
    /// The move comes first because [`Installer::rename`] puts the directory
    /// back when it cannot finish: a `SKILL.md` written before it would name a
    /// skill that is not where it says it is. If the write is what fails, the
    /// directory is moved back for the same reason.
    fn write_source(
        &mut self,
        rename_to: Option<String>,
        then: Option<Proceed>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(skill), false) = (self.skill.clone(), self.busy) else {
            return;
        };

        let text = self.body.read(cx).value().to_string();
        let dir = skill.origin.clone();
        let roots = self.roots.clone();

        self.busy = true;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let written = cx
                .background_spawn(async move {
                    let mut doc = SkillDoc::parse(&text)?;
                    let installer = Installer::new(roots.clone());
                    let mut dir = dir;
                    let mut moved = None;
                    if let Some(new_name) = &rename_to {
                        doc.frontmatter.set_name(new_name.as_str());
                        let previous = dir
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned());
                        let (landed, outcome) = installer.rename(&dir, new_name)?;
                        dir = landed;
                        moved = Some((outcome, previous));
                    }

                    if let Err(cause) = Skill::new(&dir, doc).save() {
                        // The directory has moved and still holds the file it
                        // held before, which names the old skill. Put it back
                        // rather than leave the two disagreeing; if that fails
                        // too, the error below is the one worth reporting and
                        // the scan that follows shows where things are.
                        if let Some((_, Some(previous))) = &moved {
                            installer.rename(&dir, previous).ok();
                        }
                        return Err(cause.into());
                    }

                    // The remote cache is keyed by skill name, so the record
                    // has to follow the rename or the skill loses its digest
                    // and its shas. After the save, not before: the digest is
                    // taken of the directory as it now stands, frontmatter
                    // name included, and the rollback above has already run.
                    let mut cache_failure = None;
                    if let (Some(new_name), Some((_, Some(previous)))) = (&rename_to, &moved) {
                        let mut cache = RemoteCache::read(&roots);
                        if cache.rename_skill(previous, new_name, content_digest(&dir).ok()) {
                            cache_failure = cache.write(&roots).map(|error| error.to_string());
                        }
                    }

                    // Read it back: what the pane shows next is what the disk
                    // holds, not what was sent to it.
                    let file = dir.join(SKILL_FILE_NAME);
                    let after = fs::read_to_string(&file).map_err(|e| SkillError::io(&file, e))?;
                    Ok::<_, InstallError>((
                        file,
                        after,
                        moved.map(|(outcome, _)| outcome),
                        cache_failure,
                    ))
                })
                .await;

            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match written {
                    Ok((file, after, moved, cache_failure)) => {
                        this.adopt_source(&after, window, cx);
                        let name = SkillDoc::parse(&after)
                            .ok()
                            .and_then(|doc| doc.frontmatter.name().map(SharedString::from))
                            .or_else(|| this.skill.as_ref().map(|s| s.name.clone()));
                        window.push_notification(
                            Notification::success(format!(
                                "Wrote {}",
                                display_path(&file, &this.roots)
                            ))
                            .title("Saved"),
                            cx,
                        );
                        // The move named in full, the way every other move this
                        // pane makes is: the directory it landed in, and each
                        // link that followed it there.
                        if let Some(outcome) = moved {
                            window.push_notification(
                                Notification::success(outcome.describe_under(this.roots.home()))
                                    .title("Renamed"),
                                cx,
                            );
                        }
                        if let Some(reason) = cache_failure {
                            push_notice(
                                cache_failure_notification(
                                    "The skill was renamed, but its install record could not be \
                                     moved with it",
                                    &reason,
                                ),
                                window,
                                cx,
                            );
                        }
                        cx.emit(DetailEvent::Changed { select: name });
                        run_after(then, window, cx);
                    }
                    Err(error) => {
                        window.push_notification(
                            Notification::error(error.describe_under(this.roots.home()))
                                .title("Could not save"),
                            cx,
                        );
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    // ------------------------------------------------------------- mutations

    /// Run one filesystem operation on a background thread and report it.
    ///
    /// `failed` is the title a refusal is announced under, so a delete that
    /// was turned down is not headed "Deleted".
    fn run<F>(
        &mut self,
        title: &'static str,
        failed: &'static str,
        op: F,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        F: FnOnce(Installer) -> Result<Outcome, InstallError> + Send + 'static,
    {
        self.run_op(title, failed, false, op, window, cx);
    }

    /// The same, for the delete: its report carries the button that puts the
    /// skill back.
    fn run_delete<F>(&mut self, op: F, window: &mut Window, cx: &mut Context<Self>)
    where
        F: FnOnce(Installer) -> Result<Outcome, InstallError> + Send + 'static,
    {
        self.run_op("Deleted", "Could not delete", true, op, window, cx);
    }

    /// The body of [`DetailPane::run`] and [`DetailPane::run_delete`].
    fn run_op<F>(
        &mut self,
        title: &'static str,
        failed: &'static str,
        undoable: bool,
        op: F,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        F: FnOnce(Installer) -> Result<Outcome, InstallError> + Send + 'static,
    {
        if self.busy {
            return;
        }
        let roots = self.roots.clone();
        let select = self.skill.as_ref().map(|skill| skill.name.clone());
        self.busy = true;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { op(Installer::new(roots)) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                if undoable {
                    // The undo runs long after this task is over, and nothing
                    // else would notice that a directory came back, so it
                    // carries the rescan with it. The skill it puts back is
                    // the one to land on.
                    let entity = cx.entity().downgrade();
                    let restored = select.clone();
                    report_delete(
                        title,
                        failed,
                        result,
                        &this.roots,
                        Rc::new(move |_, cx| {
                            let select = restored.clone();
                            entity
                                .update(cx, |_, cx| cx.emit(DetailEvent::Changed { select }))
                                .ok();
                        }),
                        window,
                        cx,
                    );
                } else {
                    report(title, failed, result, &this.roots, window, cx);
                }
                // Delete is the one operation run from here that writes the
                // remote cache, and the slot it writes into is its own, so a
                // failure waiting in it belongs to the delete just reported.
                if let Some(reason) = take_delete_cache_failure() {
                    push_notice(
                        delete_cache_failure_notification(
                            "The skill was deleted, but its install record could not be removed",
                            &reason,
                        ),
                        window,
                        cx,
                    );
                }
                // Scan again either way: a refusal still means the interface
                // should re-read what is actually there.
                cx.emit(DetailEvent::Changed { select });
            })
            .ok();
        })
        .detach();
    }

    /// Turn one agent's presence on or off.
    ///
    /// Off, for an agent whose link is parked in its disabled directory, first
    /// moves the link back so that there is something to unlink.
    fn set_present(
        &mut self,
        agent: &'static AgentDef,
        on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(skill) = self.skill.clone() else {
            return;
        };
        let name = skill.name.to_string();
        let origin = skill.origin.clone();
        let parked = skill.parked_in(agent.id);
        let (title, failed) = if on {
            ("Linked", "Could not link")
        } else {
            ("Unlinked", "Could not unlink")
        };

        self.run(
            title,
            failed,
            move |installer| {
                let mut done = Outcome::default();
                match set_present_step(&installer, &mut done, &name, &origin, agent, on, parked) {
                    Ok(()) => Ok(done),
                    // Unparking has already moved the link out of the agent's
                    // disabled directory when `unlink` is what failed.
                    // Reporting only "Could not unlink" would leave that
                    // unsaid, and the skill is now switched *on* for an agent
                    // the user was switching it off for.
                    // `InstallError::partial` returns the plain refusal when
                    // nothing had been done, so an ordinary failure is
                    // unchanged.
                    Err(source) => Err(InstallError::partial(done, source)),
                }
            },
            window,
            cx,
        );
    }

    /// Link or unlink a whole set of agents in one write.
    ///
    /// One [`Self::run`] rather than one per agent: eight separate calls would
    /// be eight background writes, eight notifications and eight full rescans,
    /// with the pane disabled between each of them.
    fn set_present_all(
        &mut self,
        agents: Vec<&'static AgentDef>,
        on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(skill) = self.skill.clone() else {
            return;
        };
        if agents.is_empty() {
            return;
        }
        let name = skill.name.to_string();
        let origin = skill.origin.clone();
        // An agent whose link is parked in its disabled directory has nothing
        // to unlink until the link is moved back, exactly as one row's own
        // switch handles it.
        let parked: Vec<&'static AgentDef> = agents
            .iter()
            .copied()
            .filter(|agent| skill.parked_in(agent.id))
            .collect();
        let (title, failed) = if on {
            ("Linked", "Could not link")
        } else {
            ("Unlinked", "Could not unlink")
        };

        self.run(
            title,
            failed,
            move |installer| {
                let mut done = Outcome::default();
                for agent in agents {
                    let step = set_present_step(
                        &installer,
                        &mut done,
                        &name,
                        &origin,
                        agent,
                        on,
                        parked.contains(&agent),
                    );
                    // As many as fourteen agents in one write. A refusal on the
                    // ninth still leaves the first eight linked, and the
                    // notification has to list them rather than report the
                    // refusal alone.
                    if let Err(source) = step {
                        return Err(InstallError::partial(done, source));
                    }
                }
                Ok(done)
            },
            window,
            cx,
        );
    }

    /// Switch a skill off for an agent that distinguishes present from active.
    fn set_enabled(
        &mut self,
        agent: &'static AgentDef,
        on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(skill) = self.skill.clone() else {
            return;
        };
        let name = skill.name.to_string();
        let origin = skill.origin.clone();

        let (title, failed) = if on {
            ("Enabled", "Could not enable")
        } else {
            ("Disabled", "Could not disable")
        };

        self.run(
            title,
            failed,
            move |installer| {
                if on {
                    installer.enable(&name, &origin, agent)
                } else {
                    installer.disable(&name, &origin, agent)
                }
            },
            window,
            cx,
        );
    }

    /// Confirm what adopting moves, and where to.
    ///
    /// Adopting moves a real directory on one click, and the only thing that
    /// said so was a tooltip — which named the wrong directory. It gets the
    /// same confirmation shape as a delete: name the source, name the
    /// destination, say what is left behind.
    fn confirm_adopt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(skill), false) = (self.skill.clone(), self.busy) else {
            return;
        };
        let name = skill.name.clone();
        let from = display_path(&skill.origin, &self.roots);
        let to = display_path(
            &self.roots.store_dir().join(skill.name.as_ref()),
            &self.roots,
        );
        let this = cx.entity().downgrade();

        window.open_alert_dialog(cx, move |alert, _, cx| {
            let this = this.clone();
            let (from, to) = (from.clone(), to.clone());

            let description = v_flex()
                .gap_3()
                .text_sm()
                .child(div().child(format!("Moves {from} to {to}.")))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "A symlink is left at {from}, so whatever reads that path still \
                             finds the skill. Skillbase then owns the directory, and the \
                             visibility switches stop being read-only."
                        )),
                );

            alert
                .title(format!("Adopt {name}?"))
                .description(description)
                .width(px(520.))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Adopt")
                        .cancel_text("Cancel")
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    this.update(cx, |this, cx| this.adopt(window, cx)).ok();
                    true
                })
        });
    }

    fn adopt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(skill) = self.skill.clone() else {
            return;
        };
        let name = skill.name.to_string();
        let origin = skill.origin.clone();
        self.run(
            "Adopted",
            "Could not adopt",
            move |installer| installer.adopt(&name, &origin),
            window,
            cx,
        );
    }

    /// Where releasing would put the directory: the link adoption left behind.
    ///
    /// Any link outside the store will do and the first one is the one
    /// adoption wrote, but "the first one" is a rule the user cannot see, so
    /// the confirmation names the path it picked and says how many others
    /// there were.
    fn release_destination(&self, skill: &SkillView) -> Option<PathBuf> {
        skill
            .locations
            .iter()
            .find(|l| !l.path.starts_with(self.roots.store_dir()))
            .map(|l| l.path.clone())
    }

    /// Confirm what releasing moves, where to, and which link it chose.
    fn confirm_release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(skill), false) = (self.skill.clone(), self.busy) else {
            return;
        };
        let Some(dest) = self.release_destination(&skill) else {
            window.push_notification(
                Notification::error(
                    "This skill has no link outside the store, so there is nowhere to release it \
                     to. Link it to an agent first.",
                )
                .title("Cannot release"),
                cx,
            );
            return;
        };

        let name = skill.name.clone();
        let from = display_path(
            &self.roots.store_dir().join(skill.name.as_ref()),
            &self.roots,
        );
        let to = display_path(&dest, &self.roots);
        // Every other link that points at this skill. They are not moved, and
        // after the move they point at a directory that is no longer there,
        // which is the fact the tooltip never mentioned.
        let others: Vec<SharedString> = skill
            .locations
            .iter()
            .filter(|l| !l.path.starts_with(self.roots.store_dir()) && l.path != dest)
            .map(|l| display_path(&l.path, &self.roots))
            .collect();
        let this = cx.entity().downgrade();

        window.open_alert_dialog(cx, move |alert, _, cx| {
            let this = this.clone();
            let (from, to) = (from.clone(), to.clone());
            let others = others.clone();

            let description = v_flex()
                .gap_3()
                .text_sm()
                .child(div().child(format!(
                    "Moves {from} to {to}, replacing the symlink there. Skillbase stops owning \
                     the directory, and the visibility switches become read-only."
                )))
                .when(!others.is_empty(), |this| {
                    this.child(
                        v_flex()
                            .p_2()
                            .gap_1()
                            .rounded(cx.theme().radius)
                            .bg(cx.theme().warning.opacity(0.12))
                            .child(div().text_xs().text_color(cx.theme().warning).child(format!(
                                "{to} was chosen because it is the first link outside the store. \
                                 {} other link{} not moved, and {} point at the directory this \
                                 one leaves behind:",
                                others.len(),
                                if others.len() == 1 { " is" } else { "s are" },
                                if others.len() == 1 { "it will" } else { "they will" },
                            )))
                            .children(others.iter().map(|path| {
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().warning)
                                    .child(path.clone())
                            })),
                    )
                });

            alert
                .title(format!("Release {name}?"))
                .description(description)
                .width(px(520.))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Release")
                        .cancel_text("Cancel")
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    this.update(cx, |this, cx| this.release(window, cx)).ok();
                    true
                })
        });
    }

    fn release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(skill) = self.skill.clone() else {
            return;
        };
        let name = skill.name.to_string();
        let Some(dest) = self.release_destination(&skill) else {
            window.push_notification(
                Notification::error(
                    "This skill has no link outside the store, so there is nowhere to release it \
                     to. Link it to an agent first.",
                )
                .title("Cannot release"),
                cx,
            );
            return;
        };
        self.run(
            "Released",
            "Could not release",
            move |installer| installer.release(&name, &dest),
            window,
            cx,
        );
    }

    /// Count what a delete would remove, then confirm with those paths.
    pub(crate) fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(skill), false) = (self.skill.clone(), self.busy) else {
            return;
        };
        let discovered = skill.as_discovered();
        let roots = self.roots.clone();

        cx.spawn_in(window, async move |this, cx| {
            let plan = cx
                .background_spawn(async move { Installer::new(roots).plan_delete(&discovered) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.open_delete_dialog(plan, window, cx)
            })
            .ok();
        })
        .detach();
    }

    fn open_delete_dialog(
        &mut self,
        plan: DeletePlan,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let roots = self.roots.clone();
        let name = plan.name.clone();
        let this = cx.entity().downgrade();

        window.open_alert_dialog(cx, move |alert, _, cx| {
            let plan = plan.clone();
            let roots = roots.clone();
            let this = this.clone();

            // Every path, the way the consolidate confirmation lists them: a
            // delete is not reviewable against counts, and the user is the only
            // one who can tell whether a path in the list belongs to them.
            let going: Vec<SharedString> = plan
                .origin
                .iter()
                .chain(plan.links.iter())
                .chain(plan.copies.iter())
                .map(|path| display_path(path, &roots))
                .collect();
            let kept: Vec<SharedString> = plan
                .skipped
                .iter()
                .map(|path| display_path(path, &roots))
                .collect();

            // The title names the skill, so the body opens on what goes rather
            // than repeating the name a line below it. The kinds are counted
            // because a path alone does not say whether it is a directory of
            // files or a link to one; the paths are listed because the counts
            // alone cannot be checked.
            let mut kinds: Vec<String> = Vec::new();
            if plan.origin.is_some() {
                kinds.push("the directory it lives in".to_string());
            }
            match plan.link_count() {
                0 => {}
                1 => kinds.push("1 link".to_string()),
                count => kinds.push(format!("{count} links")),
            }
            match plan.copy_count() {
                0 => {}
                1 => kinds.push("1 duplicate directory".to_string()),
                count => kinds.push(format!("{count} duplicate directories")),
            }
            let summary = if kinds.is_empty() {
                "Nothing that Skillbase manages is left to remove.".to_string()
            } else {
                format!("Removes {}:", in_a_list(&kinds))
            };
            // The same sentence the marked-set dialog ends on. Without it this
            // one read as though the directory were destroyed, and said
            // nothing about the links.
            let directories = usize::from(plan.origin.is_some()) + plan.copy_count();

            let description = v_flex()
                .gap_3()
                .text_sm()
                .child(div().child(summary))
                .child(v_flex().gap_1().children(going.iter().map(|path| {
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(path.clone())
                })))
                .when(directories > 0, |this| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(delete_effect(directories)),
                    )
                })
                .when(!kept.is_empty(), |this| {
                    this.child(
                        v_flex()
                            .p_2()
                            .gap_1()
                            .rounded(cx.theme().radius)
                            .bg(cx.theme().warning.opacity(0.12))
                            .child(div().text_xs().text_color(cx.theme().warning).child(format!(
                                "{} path{} left alone, {} outside every directory Skillbase \
                                 manages:",
                                kept.len(),
                                if kept.len() == 1 { "" } else { "s" },
                                if kept.len() == 1 { "because it is" } else { "because they are" },
                            )))
                            .children(kept.iter().map(|path| {
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().warning)
                                    .child(path.clone())
                            })),
                    )
                });

            alert
                .title(format!("Delete {name}?"))
                .description(description)
                .width(px(520.))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Delete")
                        .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
                        .cancel_text("Cancel")
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    this.update(cx, |this, cx| {
                        let plan = plan.clone();
                        let roots = roots.clone();
                        this.run_delete(
                            move |installer| {
                                let result = installer.delete(&plan);
                                // The record of a skill that is gone would
                                // answer for the next skill to take its name.
                                let mut cache = RemoteCache::read(&roots);
                                if cache.forget_deleted(slice::from_ref(&plan), &result) {
                                    remember_delete_cache_failure(cache.write(&roots));
                                }
                                result
                            },
                            window,
                            cx,
                        );
                        // Nothing to land on: the skill is gone.
                        cx.emit(DetailEvent::Changed { select: None });
                    })
                    .ok();
                    true
                })
        });
    }

    /// Confirm what consolidating would replace, naming every path.
    ///
    /// Consolidation removes real directories, so it confirms; and because the
    /// counts come from a plan computed against the disk, the confirmation
    /// states them rather than estimating them.
    fn confirm_consolidate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Duplicates::Ready(plan) = &self.duplicates else {
            return;
        };
        if self.busy || plan.replace_count() == 0 {
            return;
        }

        let plan = plan.clone();
        let roots = self.roots.clone();
        let name = SharedString::from(plan.name().to_string());
        let origin = display_path(plan.origin(), &roots);
        let this = cx.entity().downgrade();

        window.open_alert_dialog(cx, move |alert, _, cx| {
            let plan = plan.clone();
            let roots = roots.clone();
            let this = this.clone();

            let replaced: Vec<SharedString> = plan
                .duplicates()
                .iter()
                .filter(|duplicate| duplicate.will_be_replaced())
                .map(|duplicate| display_path(duplicate.path(), &roots))
                .collect();
            let forced: Vec<SharedString> = plan
                .differing()
                .filter(|duplicate| duplicate.forced())
                .map(|duplicate| {
                    format!(
                        "{} — {}",
                        display_path(duplicate.path(), &roots),
                        duplicate.diff().summary()
                    )
                    .into()
                })
                .collect();
            let skipped: Vec<SharedString> = plan
                .differing()
                .filter(|duplicate| !duplicate.forced())
                .map(|duplicate| {
                    format!(
                        "{} — {}",
                        display_path(duplicate.path(), &roots),
                        duplicate.diff().summary()
                    )
                    .into()
                })
                .collect();

            let replace_count = plan.replace_count();
            let description =
                v_flex()
                    .gap_3()
                    .text_sm()
                    .child(div().child(format!(
                        "{replace_count} director{} will be deleted and replaced by a symlink to \
                     {origin}. Nothing else on disk changes.",
                        if replace_count == 1 { "y" } else { "ies" }
                    )))
                    .child(v_flex().gap_1().children(replaced.iter().map(|path| {
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(path.clone())
                    })))
                    .when(!forced.is_empty(), |this| {
                        this.child(
                        v_flex()
                            .p_2()
                            .gap_1()
                            .rounded(cx.theme().radius)
                            .bg(cx.theme().danger.opacity(0.12))
                            .child(div().text_xs().text_color(cx.theme().danger).child(format!(
                                "{} of those you marked to replace anyway. Their differences \
                                     are lost:",
                                forced.len()
                            )))
                            .children(forced.iter().map(|line| {
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().danger)
                                    .child(line.clone())
                            })),
                    )
                    })
                    .when(!skipped.is_empty(), |this| {
                        this.child(
                            v_flex()
                                .p_2()
                                .gap_1()
                                .rounded(cx.theme().radius)
                                .bg(cx.theme().warning.opacity(0.12))
                                .child(div().text_xs().text_color(cx.theme().warning).child(
                                    format!(
                                        "{} cop{} left alone, because {} differ{} from the origin:",
                                        skipped.len(),
                                        if skipped.len() == 1 { "y" } else { "ies" },
                                        if skipped.len() == 1 { "it" } else { "they" },
                                        if skipped.len() == 1 { "s" } else { "" },
                                    ),
                                ))
                                .children(skipped.iter().map(|line| {
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().warning)
                                        .child(line.clone())
                                })),
                        )
                    });

            alert
                .title(format!(
                    "Replace {replace_count} cop{} of {name}?",
                    if replace_count == 1 { "y" } else { "ies" }
                ))
                .description(description)
                .width(px(520.))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Replace")
                        .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
                        .cancel_text("Cancel")
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    this.update(cx, |this, cx| {
                        let plan = plan.clone();
                        this.run(
                            "Replaced",
                            "Could not replace",
                            move |installer| installer.consolidate(&plan),
                            window,
                            cx,
                        );
                    })
                    .ok();
                    true
                })
        });
    }

    // -------------------------------------------------------------- updating

    /// Take the copy the repository holds now.
    ///
    /// A directory that still matches what was installed is replaced without
    /// asking: nothing is lost, and the notification names what was written.
    /// Anything else stops and confirms, because the one thing an update must
    /// never do is throw away work without saying so first.
    pub(crate) fn update_skill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(skill), false) = (self.skill.clone(), self.busy) else {
            return;
        };
        // Reachable from the File menu, which is drawn whether or not this
        // skill has anywhere to update from. The header's button is only shown
        // when this holds, so the two agree.
        if !self.can_update() {
            return;
        }
        let local = match &self.remote {
            Remote::Ready(origin) => origin.local,
            _ => LocalState::Unknown,
        };
        match local {
            LocalState::Pristine => self.apply_update(window, cx),
            LocalState::Edited | LocalState::Unknown => {
                self.confirm_update(&skill, local, window, cx)
            }
        }
    }

    /// Name what the update would overwrite, and refuse until it is accepted.
    ///
    /// The same shape as consolidating a divergent duplicate: say what the
    /// difference is, do nothing by default, and take one explicit tick to go
    /// ahead. An edited skill needs that tick; a skill Skillbase simply has no
    /// record of needs the sentence but not the ceremony.
    ///
    /// The tick is held in a cell the dialog owns rather than on the pane. The
    /// builder runs while this method's caller still has the pane open for
    /// writing, so anything the builder reads out of the pane is a read of an
    /// entity that is mid-update — the panic that killed the New skill dialog.
    /// A cell is not read through the entity at all, and it is a fresh one on
    /// every open, so an acknowledgement still cannot carry over to another
    /// skill or another day.
    fn confirm_update(
        &mut self,
        skill: &SkillView,
        local: LocalState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // One cell per dialog, so it starts unticked and nothing else can see
        // it. Shared with the checkbox that writes it and the footer that reads
        // it, both of which live for as long as the dialog does.
        let acknowledged = Rc::new(Cell::new(false));

        let name = skill.name.clone();
        let dir = display_path(&skill.origin, &self.roots);
        let repo: SharedString = skill
            .provenance
            .as_ref()
            .and_then(repo_ref_of)
            .map(|repo| repo.slug().into())
            .unwrap_or_else(|| "the repository it came from".into());
        let edited = local == LocalState::Edited;
        let upstream = self.upstream_link();
        // Named as a directory, not as the file that will land in it: the
        // installer stamps a timestamp onto the name it moves the old copy to,
        // and only the notification that follows can say which one.
        let trash = display_path(&self.roots.trash_dir(), &self.roots);
        let this = cx.entity().downgrade();

        // The builder runs on every frame the dialog is up, so the confirm
        // button's disabled state is read from the cell each time rather than
        // captured once when the dialog opened.
        window.open_dialog(cx, move |dialog, _, _| {
            // Says what happens to the directory and where it goes.
            // `replace_existing` in `skillbase-core` routes through
            // `Installer::remove_real_dir`, which moves the old directory into
            // `~/.skillbase/trash` rather than deleting it, so the recovery
            // path is one this pane can name — and the notification after a
            // successful update names the exact directory it landed in.
            let consequence = if edited {
                format!(
                    "This copy has been edited since it was installed. Updating moves {dir} to \
                     {trash} and writes the copy from {repo} in its place. Nothing is merged, so \
                     the edits stay in the moved copy and nowhere else."
                )
            } else {
                format!(
                    "Skillbase has no record of what was installed here, so it cannot tell \
                     whether this copy has been edited. Updating moves {dir} to {trash} and \
                     writes the copy from {repo} in its place."
                )
            };
            let this = this.clone();
            let upstream = upstream.clone();
            let acknowledged = acknowledged.clone();

            dialog
                .title(format!("Update {name}?"))
                .width(px(480.))
                .content({
                    let this = this.clone();
                    let acknowledged = acknowledged.clone();
                    move |content, _, _| {
                        let ticked = acknowledged.get();
                        let this = this.clone();
                        let acknowledged = acknowledged.clone();
                        content.child(
                            v_flex()
                                .p_4()
                                .gap_3()
                                .child(div().text_sm().child(consequence.clone()))
                                // Above the checkbox, because agreeing to put
                                // an edited copy in the trash is a decision the
                                // user can only make after seeing what they
                                // would be taking in exchange.
                                .when_some(upstream.clone(), |content, upstream| {
                                    content.child(upstream_element("compare-in-update", upstream))
                                })
                                .when(edited, |content| {
                                    content.child(
                                        Checkbox::new("acknowledge-update")
                                            .checked(ticked)
                                            // Not "discarding those edits":
                                            // they go to the trash, and the
                                            // sentence above says where.
                                            .label("Replace anyway")
                                            .on_click(move |checked: &bool, _, cx| {
                                                acknowledged.set(*checked);
                                                // Nothing on the pane changed,
                                                // but the dialog is redrawn
                                                // with the pane, and the tick
                                                // and the Replace button have
                                                // to follow the click.
                                                this.update(cx, |_, cx| cx.notify()).ok();
                                            }),
                                    )
                                }),
                        )
                    }
                })
                .footer({
                    let this = this.clone();
                    let blocked = edited && !acknowledged.get();
                    DialogFooter::new()
                        .p_4()
                        .child(
                            DialogClose::new()
                                .child(Button::new("cancel-update").outline().label("Cancel")),
                        )
                        .child(
                            Button::new("confirm-update")
                                .danger()
                                .label("Replace")
                                .disabled(blocked)
                                .on_click(move |_, window, cx| {
                                    this.update(cx, |this, cx| this.apply_update(window, cx))
                                        .ok();
                                    window.close_dialog(cx);
                                }),
                        )
                })
        });
    }

    /// Download the skill again and write it over its own directory.
    ///
    /// `replacing()` is what makes it a replacement rather than a refusal, and
    /// `named` keeps the directory name it already has, so a frontmatter name
    /// that has drifted from the directory cannot leave two copies behind.
    fn apply_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(skill), false) = (self.skill.clone(), self.busy) else {
            return;
        };
        let Some(location) = skill.provenance.as_ref().and_then(location_of) else {
            window.push_notification(
                Notification::error(
                    "This skill does not record a GitHub repository, so there is nothing to \
                     download.",
                )
                .title("Cannot update"),
                cx,
            );
            return;
        };

        let name = skill.name.clone();
        let roots = self.roots.clone();
        self.busy = true;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let options = InstallOptions::new().named(name.to_string()).replacing();
            let installed = cx
                .background_spawn(async move { install_skill(&roots, &location, &options) })
                .await;

            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                // Read before the result is handed over, because the change
                // list goes with it. The confirmation promised the old copy
                // would go to `~/.skillbase/trash`; this is the half of that
                // promise the confirmation could not make, because the
                // installer stamps the directory it lands in with a timestamp
                // it does not choose until the move. Nothing is said when
                // there was nothing to replace, such as a first install.
                let kept = installed
                    .as_ref()
                    .ok()
                    .and_then(|installed| {
                        installed
                            .outcome
                            .changes
                            .iter()
                            .find_map(|change| match change {
                                Change::MovedToTrash { to, .. } => Some(to.clone()),
                                _ => None,
                            })
                    })
                    .map(|path| display_path(&path, &this.roots));

                let landed = report_install(
                    "Updated",
                    "Could not update",
                    installed,
                    &this.roots,
                    window,
                    cx,
                );
                if let Some(path) = kept {
                    window.push_notification(
                        Notification::info(format!("The copy it replaced is at {path}."))
                            .title("Previous copy kept"),
                        cx,
                    );
                }
                match landed {
                    Some(select) => cx.emit(DetailEvent::Updated {
                        select: Some(select),
                    }),
                    // Nothing was written, but the pane should still re-read
                    // what is actually there rather than assume.
                    None => cx.emit(DetailEvent::Changed {
                        select: Some(name.clone()),
                    }),
                }
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn reveal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(skill) = self.skill.clone() else {
            return;
        };
        self.reveal_path(skill.origin.clone(), window, cx);
    }

    /// Reveal one directory, which is not always the selected skill's own: a
    /// duplicate row reveals the copy it names.
    fn reveal_path(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { open_in_file_manager(&dir) })
                .await;
            if let Err(error) = result {
                this.update_in(cx, |_, window, cx| {
                    window.push_notification(
                        Notification::error(error).title("Could not reveal the folder"),
                        cx,
                    );
                })
                .ok();
            }
        })
        .detach();
    }

    /// Open the skill directory in the user's own editor.
    ///
    /// A skill with reference files, scripts and a long `SKILL.md` is more work
    /// than one pane of tabs is built for, and the editor the user already has
    /// open is where that work belongs.
    fn edit_externally(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(skill) = self.skill.clone() else {
            return;
        };
        let dir = skill.origin.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { open_in_editor(&dir) })
                .await;
            if let Err(error) = result {
                this.update_in(cx, |_, window, cx| {
                    window.push_notification(
                        Notification::error(error).title("Could not open the editor"),
                        cx,
                    );
                })
                .ok();
            }
        })
        .detach();
    }

    // ----------------------------------------------------------- presentation

    fn empty_state(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        // No scan has landed yet, so the list beside this pane is still
        // skeletons: there is nothing to pick from and saying so would be a
        // lie. Once a scan is in, an empty pane means the scope is empty and
        // the sentence points somewhere real.
        let scanning = self.scan.is_none();
        // On a machine with no skills at all, telling the reader to pick one
        // from the list points at an empty list. The list's own first-run text
        // says what a skill is and offers Discover; this says the same thing
        // about the same machine so the two panes do not disagree.
        let nothing_yet = self
            .scan
            .as_ref()
            .is_some_and(|scan| scan.skills.is_empty());

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            // The window owns its titlebar drag (`app_owns_titlebar_drag`), so
            // AppKit provides no fallback region: every state has to draw the
            // band itself or the top of the window stops dragging and zooming.
            // This one carries no controls — there is no skill to act on — but
            // it matches the header's height and inset so the band across the
            // title row stays continuous.
            .child(
                drag_band("detail-header", window, cx)
                    .flex_shrink_0()
                    .h(BAND_HEIGHT)
                    .px_5()
                    .gap_3()
                    .items_center()
                    .justify_between(),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(
                        Icon::new(IconName::FileText)
                            .large()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(div().text_sm().font_medium().child(if nothing_yet {
                        "No skills yet"
                    } else {
                        "No skill selected"
                    }))
                    .when(!scanning, |this| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .max_w(px(PROSE_MAX_WIDTH))
                                .text_center()
                                .child(if nothing_yet {
                                    "A skill is a folder with a SKILL.md file in it that tells \
                                     an agent how to do one thing. This pane reads and edits \
                                     the one you pick."
                                } else {
                                    "Pick a skill from the list to read or edit it."
                                }),
                        )
                    }),
            )
            .into_any_element()
    }

    /// The 48px band across the top of the pane.
    ///
    /// It carries the tab strip on its left, inside the window's own drag
    /// region, and the action buttons — Reveal, Delete, Update when there is
    /// somewhere for it to land, and Save — on its right. The buttons are
    /// `flex_shrink_0` so a strip long enough to overrun the band is what
    /// gives way and scrolls; Save never gets squeezed out to make room for a
    /// tab.
    fn header(
        &self,
        skill: &SkillView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        drag_band("detail-header", window, cx)
            .flex_shrink_0()
            .h(BAND_HEIGHT)
            .px_5()
            .gap_2()
            .items_center()
            .justify_between()
            .child(self.tabs(cx))
            .child(
                h_flex()
                    .flex_shrink_0()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("reveal")
                            .ghost()
                            .small()
                            .icon(IconName::FolderOpen)
                            .tooltip("Reveal the origin folder")
                            // Icon-only: a Button names itself from its label
                            // or this, never from its tooltip.
                            .accessibility_label("Reveal the origin folder")
                            .on_click(cx.listener(|this, _, window, cx| this.reveal(window, cx))),
                    )
                    .child(
                        Button::new("delete")
                            .ghost()
                            .small()
                            .icon(IconName::Delete)
                            .tooltip("Delete this skill")
                            .accessibility_label("Delete this skill")
                            .disabled(self.busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.confirm_delete(window, cx)),
                            ),
                    )
                    // Only when there is something to take, and only when it
                    // would land on this skill's own directory. Outline rather
                    // than primary: Save is the commit this header is built
                    // around, and two emphasised buttons would compete.
                    .when(
                        self.update_available() && self.updates_in_place(skill),
                        |this| {
                            this.child(
                                Button::new("update")
                                    .outline()
                                    .small()
                                    .label("Update")
                                    .tooltip("Replace this skill with the copy upstream holds")
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.update_skill(window, cx)
                                    })),
                            )
                        },
                    )
                    .child(self.actions_menu(cx))
                    .child(
                        Button::new("save")
                            .primary()
                            .small()
                            .label("Save")
                            .disabled(!self.showing_dirty() || self.busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.save_active(window, cx)),
                            ),
                    ),
            )
    }

    /// The pane's overflow menu: what acts on this skill but is not worth a
    /// button of its own in a 48px band.
    ///
    /// Renaming is here rather than on the Overview because it is not an edit
    /// to a field. It moves the skill's directory, which is why editing `name:`
    /// in the `SKILL.md` tab does not do it.
    fn actions_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let busy = self.busy;

        Button::new("detail-actions")
            .ghost()
            .small()
            .icon(IconName::Ellipsis)
            .tooltip("More actions")
            .accessibility_label("More actions")
            .dropdown_menu(move |menu, _, _| {
                let (rename, edit) = (this.clone(), this.clone());
                menu.min_w(px(200.))
                    .item(PopupMenuItem::new("Rename…").disabled(busy).on_click(
                        move |_, window, cx| {
                            rename
                                .update(cx, |this, cx| this.open_rename(window, cx))
                                .ok();
                        },
                    ))
                    .item(
                        // The same command the Location section offers. It is
                        // here as well because Location is on the Overview,
                        // and this band is on every tab.
                        PopupMenuItem::new("Open in editor").on_click(move |_, window, cx| {
                            edit.update(cx, |this, cx| this.edit_externally(window, cx))
                                .ok();
                        }),
                    )
            })
    }

    /// The 44px row under the band: the skill's name, a warning glyph when
    /// its frontmatter fails to parse, and a tag when it is unmanaged.
    ///
    /// Only when it is unmanaged. Managed is the ordinary case — it is what
    /// every skill Skillbase installed or created is — and a badge on almost
    /// every skill in the library says nothing about the one being read. The
    /// exception is worth a word; the rule is not.
    ///
    /// This used to sit in the band itself, next to the action buttons. The
    /// band's left side is the tab strip now, so the name needs a row of its
    /// own — one that stays put under the tabs rather than scrolling with
    /// them.
    fn identity_row(&self, skill: &SkillView, cx: &mut Context<Self>) -> impl IntoElement {
        let invalid = skill.parse_error.is_some();

        h_flex()
            .flex_shrink_0()
            .h_11()
            .px_5()
            .gap_2()
            .items_center()
            .child(
                div()
                    .text_base()
                    .font_medium()
                    .truncate()
                    .child(skill.name.clone()),
            )
            .when(invalid, |this| {
                this.child(
                    Icon::new(IconName::TriangleAlert)
                        .small()
                        .text_color(cx.theme().warning),
                )
            })
            .when(!skill.managed, |this| {
                // "Unmanaged" on its own does not say what follows from it.
                // The `?` beside it does, and Location below offers Adopt.
                this.child(Tag::secondary().small().child("Unmanaged"))
                    .child(help_dot(
                        ElementId::from("unmanaged-help"),
                        "The directory is not in Skillbase's store, so another tool may own it. \
                         Links and visibility are read-only until you adopt it."
                            .to_string(),
                        cx,
                    ))
            })
    }

    /// The "Visible to" section, closed until asked for.
    ///
    /// It used to sit open between the description and the editor, a column of
    /// a dozen switches and a paragraph of explanation under each. Editing the
    /// file is what this pane is mostly for, and that column pushed the editor
    /// off the bottom of the window. Closed, the header still answers the
    /// question the section exists to answer — who can see this — and opening
    /// it is one click.
    ///
    /// Open, it is a row per agent and nothing else. The paragraphs are gone:
    /// what a switch does is on the switch, and what a row's own state means
    /// is on the `?` beside its name. Fourteen rows of prose said the same
    /// three sentences over and over, differing in one path segment.
    fn visibility(&self, skill: &SkillView, cx: &mut Context<Self>) -> impl IntoElement {
        let managed = skill.managed;
        let open = self.visibility_open;
        let shared = Registry::shared();

        // Agents on this machine, plus any that already hold a link to this
        // skill. The scan decides what "on this machine" means, once, for the
        // sidebar and this pane alike. Zed gets no row of its own: its
        // directory is the shared directory, so linking "into Zed" is the
        // Shared switch.
        let installed = self
            .scan
            .as_ref()
            .map(|scan| scan.installed.clone())
            .unwrap_or_default();
        let agents: Vec<&'static AgentDef> = Registry::link_targets()
            .filter(|agent| !agent.is_shared())
            .filter(|agent| installed.contains(agent) || skill.linked_to(agent.id))
            .collect();
        // The other side of that filter. It used to be dropped silently, so a
        // user looking for Cursor found no row and no sentence saying why.
        let absent: Vec<&'static str> = Registry::link_targets()
            .filter(|agent| !agent.is_shared())
            .filter(|agent| !installed.contains(agent) && !skill.linked_to(agent.id))
            .map(|agent| agent.display_name)
            .collect();

        v_flex()
            .flex_shrink_0()
            .gap_3()
            // A Button rather than a hand-rolled row: a disclosure is a
            // control, and only a Button here carries a focus handle, so only
            // a Button answers Enter and Space and draws a focus ring. The
            // chevron and the summary go in as children rather than as
            // `.icon()` and `.label()`, which would take the button's own size
            // and colour instead of the heading's.
            .child(
                Button::new("visibility-header")
                    .ghost()
                    .small()
                    .w_full()
                    .accessibility_label("Visible to")
                    // The chevron is the only thing that says open or closed,
                    // and a listener cannot see it.
                    .toggled(open)
                    .child(
                        Icon::new(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .xsmall()
                        .flex_shrink_0()
                        .text_color(cx.theme().muted_foreground),
                    )
                    .child(section_title("Visible to", cx))
                    .child(div().flex_1().min_w_0())
                    .child(
                        div()
                            .min_w_0()
                            .text_xs()
                            .truncate()
                            .text_color(cx.theme().muted_foreground)
                            .child(reach(skill, &installed, cx)),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.visibility_open = !this.visibility_open;
                        cx.notify();
                    })),
            )
            .when(open, |this| {
                let covered: Vec<&'static str> = Registry::covered_by_shared()
                    .map(|agent| agent.display_name)
                    .collect();
                let shared_path = display_path(
                    &self.roots.shared_dir().join(skill.name.as_ref()),
                    &self.roots,
                );
                let private_path = display_path(
                    &self.roots.private_dir().join(skill.name.as_ref()),
                    &self.roots,
                );
                // Shared is the one switch that moves the directory rather
                // than adding or removing a link, because the shared directory
                // is the store. Say which way it will move, where to, and
                // which agents read it — on the `?`, because it is three
                // facts and none of them changes until the switch is thrown.
                let shared_effect = if skill.in_shared {
                    format!(
                        "Lives at {shared_path}, which {} agents read: {}. Switching off moves \
                         it to {private_path}.",
                        covered.len(),
                        covered.join(", ")
                    )
                } else {
                    format!(
                        "Lives at {private_path}. Switching on moves it back to {shared_path}, \
                         which {} agents read: {}.",
                        covered.len(),
                        covered.join(", ")
                    )
                };

                this.child(
                    v_flex()
                        .gap_3()
                        .when(!managed, |this| {
                            this.child(
                                div()
                                    .p_3()
                                    .rounded(cx.theme().radius)
                                    .bg(cx.theme().group_box)
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    // The measure is capped on the text, not
                                    // on the box: the box stays the width of
                                    // the rows it sits above.
                                    .child(div().max_w(px(PROSE_MAX_WIDTH)).child(
                                        "This skill's directory is not in Skillbase's store, \
                                             so another tool may own it. Visibility is read-only \
                                             until you adopt it.",
                                    )),
                            )
                        })
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    Switch::new("visible-shared")
                                        .small()
                                        .checked(skill.in_shared)
                                        .disabled(!managed || self.busy)
                                        .label("Shared")
                                        .tooltip(shared_path.clone())
                                        .on_click(cx.listener(
                                            move |this, checked: &bool, window, cx| {
                                                this.set_present(shared, *checked, window, cx)
                                            },
                                        )),
                                )
                                .child(help_dot(ElementId::from("shared-help"), shared_effect, cx)),
                        )
                        // Nothing to act on when no agent below needs a link
                        // of its own; a pair of dead buttons over an empty
                        // list would be worse than no control.
                        .when(!agents.is_empty(), |this| {
                            this.child(self.link_all_row(skill, &agents, cx))
                        })
                        .child(
                            v_flex().gap_2().children(
                                agents
                                    .iter()
                                    .copied()
                                    .map(|agent| self.agent_row(skill, agent, cx)),
                            ),
                        )
                        .when(!absent.is_empty(), |this| {
                            this.child(
                                div()
                                    .max_w(px(PROSE_MAX_WIDTH))
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(absent_sentence(&absent)),
                            )
                        }),
                )
            })
    }

    /// The one control above the agent rows that acts on all of them.
    ///
    /// Giving a skill to every agent used to be a click per row, and each
    /// click was a background write and a full rescan with the pane disabled
    /// in between. These two buttons do the whole set in one operation, one
    /// notification and one scan.
    ///
    /// "All" is the agents listed below, minus the ones already reached
    /// through Shared: linking those would write a link that changes nothing
    /// and contradict what their own row says. Each button's tooltip names the
    /// agents it would act on, which is the only honest answer to "all of
    /// what" — and it replaced a caption that said the same thing in prose
    /// above two buttons that could say it themselves.
    fn link_all_row(
        &self,
        skill: &SkillView,
        agents: &[&'static AgentDef],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // An agent that reads the shared directory already sees the skill, so
        // linking it would write a link that changes nothing and contradict
        // the note in its own row. "All" here means the agents that are not
        // reached yet.
        let to_link: Vec<&'static AgentDef> = agents
            .iter()
            .copied()
            .filter(|agent| !skill.linked_to(agent.id) && !skill.via_shared(agent))
            .collect();
        let to_unlink: Vec<&'static AgentDef> = agents
            .iter()
            .copied()
            .filter(|agent| skill.linked_to(agent.id))
            .collect();
        // The tooltips name the agents rather than counting them: "Link all"
        // is only safe to press when the reader can see what "all" is.
        let link_names = names_of(&to_link);
        let unlink_names = names_of(&to_unlink);
        let locked = !skill.managed || self.busy;

        h_flex()
            .gap_2()
            .items_center()
            .justify_end()
            .child(
                Button::new("link-all")
                    .outline()
                    .small()
                    .label("Link all")
                    .tooltip(if link_names.is_empty() {
                        "Every agent below already reaches this skill".to_string()
                    } else {
                        format!(
                            "Links {link_names}, in one write. The agents reached through Shared \
                             are left alone."
                        )
                    })
                    .disabled(locked || to_link.is_empty())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_present_all(to_link.clone(), true, window, cx)
                    })),
            )
            .child(
                Button::new("unlink-all")
                    .outline()
                    .small()
                    .label("Unlink all")
                    .tooltip(if unlink_names.is_empty() {
                        "No agent below has a link to remove".to_string()
                    } else {
                        format!("Removes the links at {unlink_names}, in one write")
                    })
                    .disabled(locked || to_unlink.is_empty())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_present_all(to_unlink.clone(), false, window, cx)
                    })),
            )
    }

    /// One agent's row: its mark and name, and the one or two switches that
    /// change what it can see. One line, always.
    ///
    /// Presence and "switched on" are different questions for Claude Code and
    /// Codex, so they get two switches, side by side in fixed lanes. What
    /// turning each off does is a tooltip on the switch it belongs to.
    ///
    /// Nothing under the row. "Linked at ~/.claude/skills/foo" under a dozen
    /// rows is a page of near-identical text differing in one path segment, so
    /// it is a tooltip on the row itself. The two states a switch cannot say —
    /// a copy that is not a link, and a directory parked out of the way — get
    /// a `?` next to the agent's name instead, which is a glyph rather than a
    /// paragraph and still holds the whole sentence.
    ///
    /// The other two states used to have notes of their own and no longer do.
    /// An agent reached through Shared, and one reached through Shared as well
    /// as by its own link, are both things the Linked switch is already
    /// hovered to ask about, and its tooltip now answers there.
    fn agent_row(
        &self,
        skill: &SkillView,
        agent: &'static AgentDef,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let managed = skill.managed;
        let present = skill.linked_to(agent.id);
        // `via_shared` goes false the moment the agent has a link of its own,
        // so these two are exclusive: reached only through Shared, and reached
        // through Shared as well as by a link.
        let via_shared = skill.via_shared(agent);
        let also_shared = present && agent.reads_shared && skill.in_shared;
        let dir = display_path(
            &self.roots.agent_dir(agent).join(skill.name.as_ref()),
            &self.roots,
        );

        let kind = skill.location_kind(agent.id);
        // The two states neither switch can say, and the only prose left in a
        // row: a copy is not a link, and a parked directory is not where the
        // switch says it is. Both go on the `?`.
        let note: Option<String> = match kind {
            Some(LocationKind::Copy) => Some(format!(
                "A separate copy at {dir}, not a link. Editing the skill here does not change it."
            )),
            Some(LocationKind::Disabled) => Some(format!(
                "Parked out of the way; {dir} is empty. The Enabled switch puts it back."
            )),
            _ => None,
        };
        let effect = match kind {
            Some(LocationKind::Origin) => format!("The origin directory itself, at {dir}"),
            Some(LocationKind::Symlink { .. }) => format!("Linked at {dir}"),
            _ => format!("Links {dir}"),
        };
        // What turning this switch off does. The row can carry two switches a
        // word apart, and the words alone never said which was which: Linked is
        // whether the agent has the skill at all, Enabled is whether it reads
        // the copy it has. Each switch says its own half on hover — and, for
        // the two rows Shared has a hand in, whose switch is really the
        // control.
        let unlink = if via_shared {
            format!(
                "On because {} reads the shared directory, so it sees this skill without a link \
                 of its own. The Shared switch above is what turns it off.",
                agent.display_name
            )
        } else if matches!(kind, Some(LocationKind::Origin)) {
            format!("The skill itself lives at {dir}, so there is no link here to remove.")
        } else if also_shared {
            format!(
                "Off removes the link at {dir}, but {} also reads the shared directory, so the \
                 skill stays visible to it.",
                agent.display_name
            )
        } else {
            format!("Off removes the link at {dir} and leaves the skill in the store.")
        };

        let switchable = has_disable_state(agent) && (present || via_shared);
        // Codex's off state is a line in its own config file, so writing it
        // does not disturb whoever owns the skill. Claude Code's is a move
        // between directories, which is a visibility change, and §3.2 keeps
        // those read-only for a skill Skillbase does not own.
        let locked = !managed && matches!(agent.disable, DisableMode::MoveAside(_));
        let enabled = skill.enabled_for(agent);
        let how = disable_effect(agent, &self.roots);

        h_flex()
            .id(ElementId::from((ElementId::from("agent-row"), agent.id)))
            .gap_3()
            .items_center()
            .when(note.is_none(), |this| {
                this.tooltip(move |window, cx| Tooltip::new(effect.clone()).build(window, cx))
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .items_center()
                    .child(agent_icon(agent).xsmall())
                    .child(div().min_w_0().truncate().child(agent.display_name))
                    .children(note.map(|note| {
                        help_dot(
                            ElementId::from((ElementId::from("agent-help"), agent.id)),
                            note,
                            cx,
                        )
                    })),
            )
            // Two fixed lanes, so the presence switch lands on the same
            // column in every row whether or not the agent has an
            // "enabled" state, and both switches carry their own label
            // rather than sharing one between them.
            //
            // Sized for the small switch: its track is 8px narrower
            // than medium, and its label sits at text_sm instead of
            // text_base, so both lanes shrank by a rem from the
            // widths a medium switch needed.
            .child(
                h_flex()
                    .flex_shrink_0()
                    .w(rems(5.5))
                    .justify_end()
                    .when(switchable, |this| {
                        this.child(
                            Switch::new((ElementId::from("enabled"), agent.id))
                                .small()
                                .checked(enabled)
                                .disabled(locked || self.busy)
                                .label("Enabled")
                                // The visible label has room for one word;
                                // a reader who cannot see which row it sits
                                // in needs the agent named, and this
                                // switch moves files on disk.
                                .accessibility_label(format!("{} enabled", agent.display_name))
                                .tooltip(how)
                                .on_click(cx.listener(move |this, checked: &bool, window, cx| {
                                    this.set_enabled(agent, *checked, window, cx)
                                })),
                        )
                    }),
            )
            .child(
                h_flex().flex_shrink_0().w(rems(4.5)).justify_end().child(
                    // "Linked", not "Visible": this switch adds or
                    // removes the link, which is the word the row's
                    // own caption and its notification already use.
                    // "Visible" and "Enabled" side by side read as the
                    // same question asked twice.
                    Switch::new((ElementId::from("visible"), agent.id))
                        .small()
                        // On for an agent that is reached through
                        // Shared, because it is. The section header
                        // has always counted those agents as reached;
                        // the row used to sit at Off beside it and say
                        // the opposite about the same agent.
                        .checked(present || via_shared)
                        // ...and not the control that put it there, so
                        // it does not offer to change it. Its own
                        // tooltip says where the control is.
                        .disabled(!managed || self.busy || via_shared)
                        .label("Linked")
                        .tooltip(unlink)
                        .accessibility_label(if via_shared {
                            format!(
                                "{} reached through Shared, not linked separately",
                                agent.display_name
                            )
                        } else {
                            format!("{} linked", agent.display_name)
                        })
                        .on_click(cx.listener(move |this, checked: &bool, window, cx| {
                            // What is written to disk is unchanged.
                            // The switch is clickable only where what
                            // it shows *is* the link state, so
                            // `checked` is never the Shared reading
                            // and this cannot write a link the row
                            // did not ask for.
                            this.set_present(agent, *checked, window, cx)
                        })),
                ),
            )
            .into_any_element()
    }

    /// Where the skill came from, and how it stands against what is there now.
    ///
    /// Nothing at all for a skill with no provenance, which on a real machine
    /// is most of them. An empty Source box under every hand-written skill
    /// would be a heading standing in for an answer nobody asked for.
    fn source_section(&self, skill: &SkillView, cx: &mut Context<Self>) -> AnyElement {
        let origin = match &self.remote {
            Remote::None => return div().into_any_element(),
            Remote::Reading => {
                return v_flex()
                    .flex_shrink_0()
                    .gap_2()
                    .child(section_title("Source", cx))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("Reading where this skill came from…"),
                    )
                    .into_any_element();
            }
            Remote::Ready(origin) => origin,
        };

        let provenance = &origin.provenance;
        let repo: SharedString = repo_ref_of(provenance)
            .map(|repo| repo.slug().into())
            .unwrap_or_else(|| provenance.repo_url.clone().into());
        let path: SharedString = provenance.path.clone().into();
        let elsewhere = !self.updates_in_place(skill);

        v_flex()
            .flex_shrink_0()
            .gap_2()
            .child(section_title("Source", cx))
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .min_w_0()
                            .text_sm()
                            .child(Icon::new(IconName::Github).xsmall())
                            .child(div().flex_1().min_w_0().truncate().child(repo)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(if provenance.path.is_empty() {
                                provenance.reference.clone()
                            } else {
                                format!("{} · {}", provenance.reference, path)
                            }),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(if self.update_available() {
                                cx.theme().foreground
                            } else {
                                cx.theme().muted_foreground
                            })
                            .child(self.update_sentence(&provenance.reference)),
                    )
                    // The only place in this section that shows what the
                    // update actually is. A `Link`, not a Button: it leaves
                    // the application for github.com.
                    .when_some(self.upstream_link(), |this, upstream| {
                        this.child(upstream_element("compare-upstream", upstream))
                    })
                    .when(origin.local == LocalState::Edited, |this| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().warning)
                                .child("Edited locally since install."),
                        )
                    })
                    .when(origin.local == LocalState::Unknown, |this| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child("No install record, so local edits cannot be told."),
                        )
                    })
                    .when(origin.checked_at > 0, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("Checked {}", ago(origin.checked_at))),
                        )
                    })
                    .when(elsewhere, |this| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().warning)
                                .child(format!(
                                    "An update would land in {}, not this directory.",
                                    display_path(
                                        &self.roots.store_dir().join(skill.name.as_ref()),
                                        &self.roots
                                    )
                                )),
                        )
                    }),
            )
            .into_any_element()
    }

    /// The last check's answer, in a sentence.
    ///
    /// Every branch names what would change and what it was measured against.
    /// A status with no words behind it is worse than no status: the reader
    /// cannot tell a check that came back clean from one that never ran.
    fn update_sentence(&self, reference: &str) -> SharedString {
        match self.update_status() {
            None => "Not checked against GitHub yet.".into(),
            Some(UpdateStatus::Unknown) => "The recorded source is not a GitHub repository, so \
                                            there is nothing to check it against."
                .into(),
            Some(UpdateStatus::UpToDate) => format!("Up to date with {reference}.").into(),
            Some(UpdateStatus::UpdateAvailable { tree_sha }) => format!(
                "An update is available. {reference} holds {} now.",
                short_sha(tree_sha)
            )
            .into(),
            Some(UpdateStatus::NoBaseline { tree_sha }) => format!(
                "Whatever installed this recorded no tree sha, so whether it is current cannot \
                 be told from the record. {reference} holds {} now.",
                short_sha(tree_sha)
            )
            .into(),
            Some(UpdateStatus::Gone) => "The repository no longer holds a directory at this \
                                         path. It was renamed, moved or removed upstream."
                .into(),
            Some(UpdateStatus::Failed {
                rate_limited: true, ..
            }) => "Not checked: GitHub's request limit was spent. Settings says when it resets."
                .into(),
            Some(UpdateStatus::Failed { reason, .. }) => {
                format!("Could not be checked: {reason}").into()
            }
            // `UpdateStatus` is `#[non_exhaustive]`: a status this build does
            // not know about is reported as unchecked rather than guessed at.
            Some(_) => "Not checked against GitHub yet.".into(),
        }
    }

    fn location(&self, skill: &SkillView, cx: &mut Context<Self>) -> impl IntoElement {
        let origin = display_path(&skill.origin, &self.roots);
        let conflicts = skill.conflicts.len();

        v_flex()
            .flex_shrink_0()
            .gap_2()
            .child(section_title("Location", cx))
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .min_w_0()
                            .text_sm()
                            .child(Icon::new(IconName::Folder).xsmall())
                            .child(div().flex_1().min_w_0().truncate().child(origin)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(if skill.managed {
                                "Owned by Skillbase. Links can be added or removed."
                            } else {
                                "Edited in place. Adopt moves it into the store and leaves a \
                                 symlink behind."
                            }),
                    )
                    .when(conflicts > 0, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().warning)
                                .child(format!(
                                    "{conflicts} other real director{} claim{} this name. Editing \
                                 here does not change {}. They are listed under Duplicate \
                                 copies below.",
                                    if conflicts == 1 { "y" } else { "ies" },
                                    if conflicts == 1 { "s" } else { "" },
                                    if conflicts == 1 { "it" } else { "them" },
                                )),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("reveal-folder")
                                    .outline()
                                    .small()
                                    .icon(IconName::FolderOpen)
                                    // The macOS gesture selects the folder in
                                    // its parent window; elsewhere the file
                                    // manager opens the folder itself, and the
                                    // label says which one it is.
                                    .label(if cfg!(target_os = "macos") {
                                        "Reveal in Finder"
                                    } else {
                                        "Open folder"
                                    })
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.reveal(window, cx)),
                                    ),
                            )
                            .child(
                                Button::new("open-in-editor")
                                    .outline()
                                    .small()
                                    .icon(IconName::SquareTerminal)
                                    .label("Open in editor")
                                    .tooltip("Open the whole directory with $VISUAL or $EDITOR")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.edit_externally(window, cx)
                                    })),
                            )
                            // Both open a confirmation, so both take the
                            // ellipsis: each moves a real directory, and the
                            // dialog is where the source and destination are
                            // named.
                            .child(if skill.managed {
                                Button::new("release")
                                    .outline()
                                    .small()
                                    .label("Release…")
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.confirm_release(window, cx)
                                    }))
                            } else {
                                Button::new("adopt")
                                    .primary()
                                    .small()
                                    .label("Adopt…")
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.confirm_adopt(window, cx)
                                    }))
                            }),
                    ),
            )
    }

    /// Every real directory that duplicates this skill, and what it would take
    /// to make them one skill again.
    ///
    /// A machine that has been through several agents ends up with the same
    /// skill copied into eight directories. They are copies, not links, so
    /// they drift: editing one changes nothing for the other seven. This
    /// section names each of them and offers the fix.
    fn duplicates_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let plan = match &self.duplicates {
            Duplicates::None => return div().into_any_element(),
            Duplicates::Comparing => {
                return v_flex()
                    .flex_shrink_0()
                    .gap_2()
                    .child(section_title("Duplicate copies", cx))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("Comparing the copies against the origin…"),
                    )
                    .into_any_element();
            }
            Duplicates::Ready(plan) => plan,
        };

        let origin = display_path(plan.origin(), &self.roots);
        let copies = plan.duplicates().len();
        let identical = plan.identical_count();
        let differing = plan.differing_count();
        let replace = plan.replace_count();

        v_flex()
            .flex_shrink_0()
            .gap_2()
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .justify_between()
                    .child(section_title("Duplicate copies", cx))
                    .child(
                        Button::new("consolidate")
                            .primary()
                            .small()
                            .label(if replace == 1 {
                                "Replace 1 copy".to_string()
                            } else {
                                format!("Replace {replace} copies")
                            })
                            .tooltip("Replace each copy with a symlink to the origin")
                            .disabled(self.busy || replace == 0)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm_consolidate(window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(duplicates_summary(copies, identical, differing, &origin)),
            )
            .child(
                v_flex()
                    .rounded(cx.theme().radius)
                    // The rows are square, and a divergent one carries a left
                    // border: both would paint over the card's corner curve.
                    .overflow_hidden()
                    .bg(cx.theme().group_box)
                    .children(
                        plan.duplicates()
                            .iter()
                            .map(|duplicate| self.duplicate_row(duplicate, cx)),
                    ),
            )
            .when(!plan.skipped().is_empty(), |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "{} path{} sit outside every directory Skillbase manages and are not \
                             touched.",
                            plan.skipped().len(),
                            if plan.skipped().len() == 1 { "" } else { "s" }
                        )),
                )
            })
            .into_any_element()
    }

    fn duplicate_row(&self, duplicate: &Duplicate, cx: &mut Context<Self>) -> AnyElement {
        let path = duplicate.path().to_path_buf();
        let id = SharedString::from(path.display().to_string());
        let shown = display_path(&path, &self.roots);
        let identical = duplicate.is_identical();
        let forced = duplicate.forced();
        // A divergent copy is the one thing on this screen that is about to be
        // lost, so it carries the warning colour and the origin's colour is
        // left alone.
        let status_color = if identical {
            cx.theme().muted_foreground
        } else if forced {
            cx.theme().danger
        } else {
            cx.theme().warning
        };

        v_flex()
            .id(ElementId::from((ElementId::from("duplicate"), id.clone())))
            .w_full()
            .px_3()
            .py_2()
            .gap_1()
            .when(!identical, |this| {
                this.border_l_2().border_color(status_color)
            })
            .child(
                h_flex()
                    .w_full()
                    .gap_3()
                    .items_center()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .w_40()
                            .flex_shrink_0()
                            .child(
                                Icon::new(if identical {
                                    IconName::Copy
                                } else {
                                    IconName::TriangleAlert
                                })
                                .xsmall()
                                .text_color(status_color),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .truncate()
                                    .child(scope_label(duplicate.agent_id())),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(shown.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(status_color)
                            .child(if identical {
                                "matches the origin".to_string()
                            } else {
                                duplicate.diff().summary()
                            }),
                    )
                    // The origin has a Reveal button in Location; a copy about
                    // to be replaced had none, so the only way to see what was
                    // in it was to retype the path somewhere else.
                    .child(
                        Button::new(ElementId::from((
                            ElementId::from("reveal-duplicate"),
                            id.clone(),
                        )))
                        .ghost()
                        .xsmall()
                        .flex_shrink_0()
                        .icon(IconName::FolderOpen)
                        .tooltip(if cfg!(target_os = "macos") {
                            "Reveal this copy in Finder"
                        } else {
                            "Open this copy's folder"
                        })
                        .accessibility_label(format!("Reveal {shown}"))
                        .on_click(cx.listener({
                            let path = path.clone();
                            move |this, _, window, cx| this.reveal_path(path.clone(), window, cx)
                        })),
                    ),
            )
            .when(!identical, |this| {
                // Name the files, then offer the only way to overwrite them.
                // Not offering it would leave the user stuck; offering it
                // without naming what goes would be worse than not offering it.
                let total = duplicate.diff().total();
                let expanded = self.expanded_diffs.contains(&path);
                let shown = if expanded { total } else { DIFF_PATHS };
                let paths: Vec<SharedString> = duplicate
                    .diff()
                    .paths()
                    .take(shown)
                    .map(|path| SharedString::from(path.display().to_string()))
                    .collect();
                let hidden = total.saturating_sub(paths.len());

                this.child(
                    v_flex()
                        .pl_6()
                        .gap_1()
                        .child(
                            h_flex()
                                .flex_wrap()
                                .gap_1()
                                .items_center()
                                .children(
                                    paths
                                        .into_iter()
                                        .map(|path| Tag::secondary().xsmall().child(path)),
                                )
                                // A bare "+3" named nothing and did nothing.
                                // The rest of the list is one click away, and
                                // the same click puts it back.
                                .when(hidden > 0 || expanded, |this| {
                                    this.child(
                                        Button::new(ElementId::from((
                                            ElementId::from("more-diffs"),
                                            id.clone(),
                                        )))
                                        .ghost()
                                        .xsmall()
                                        .label(if expanded {
                                            format!("Show first {DIFF_PATHS}")
                                        } else {
                                            format!("Show {hidden} more")
                                        })
                                        .on_click(
                                            cx.listener({
                                                let path = path.clone();
                                                move |this, _, _, cx| {
                                                    this.toggle_diff_paths(&path, cx)
                                                }
                                            }),
                                        ),
                                    )
                                }),
                        )
                        .child(
                            Checkbox::new((ElementId::from("force"), id))
                                .checked(forced)
                                .disabled(self.busy)
                                .label("Replace anyway, discarding these differences")
                                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                    this.set_forced(&path, *checked, cx)
                                })),
                        ),
                )
            })
            .into_any_element()
    }

    /// Show a duplicate's whole differing-path list, or go back to the first
    /// few.
    fn toggle_diff_paths(&mut self, path: &Path, cx: &mut Context<Self>) {
        if !self.expanded_diffs.remove(path) {
            self.expanded_diffs.insert(path.to_path_buf());
        }
        cx.notify();
    }

    /// The skill directory, as rows the user can click to open a file.
    ///
    /// This replaces the row of tags that named the bundled files without
    /// letting the user do anything with them.
    ///
    /// A disclosure, in the same shape as "Visible to" above it, because the
    /// listing is not always short: `docx` is thirty rows, which is a screen
    /// and a half of the Overview for a question — what is in the directory —
    /// that most readers are not asking. A long listing therefore arrives
    /// closed, a short one open ([`files_open_by_default`]), and the heading
    /// carries the count either way so a closed one still says how much is
    /// there.
    ///
    /// Collapsing rather than scrolling in a pane of its own: the tab around it
    /// is already a scroll container, and a second one nested inside means the
    /// wheel moves whichever of the two the pointer happens to be over, which
    /// is not something the user can predict.
    fn folder_structure(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.files_open;
        let summary = files_summary(&self.tree);

        v_flex()
            .flex_shrink_0()
            .gap_2()
            // A Button rather than a hand-rolled row, for the reasons the
            // "Visible to" header gives: only a Button here carries a focus
            // handle, so only a Button answers Enter and Space.
            .child(
                Button::new("files-header")
                    .ghost()
                    .small()
                    .w_full()
                    .accessibility_label("Files")
                    // The chevron is the only thing that says open or closed,
                    // and a listener cannot see it.
                    .toggled(open)
                    .child(
                        Icon::new(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .xsmall()
                        .flex_shrink_0()
                        .text_color(cx.theme().muted_foreground),
                    )
                    .child(section_title("Files", cx))
                    .child(div().flex_1().min_w_0())
                    .when_some(summary, |this, summary| {
                        this.child(
                            div()
                                .min_w_0()
                                .text_xs()
                                .truncate()
                                .text_color(cx.theme().muted_foreground)
                                .child(summary),
                        )
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.files_open = !this.files_open;
                        cx.notify();
                    })),
            )
            .when(open, |this| {
                this.child(if self.tree.is_empty() {
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("The skill directory could not be read.")
                        .into_any_element()
                } else {
                    v_flex()
                        .py_1()
                        .rounded(cx.theme().radius)
                        // The rows are square. Without this the top and bottom
                        // row's hover and selected fills paint over the card's
                        // corner curve.
                        .overflow_hidden()
                        .bg(cx.theme().group_box)
                        .children(self.tree.clone().iter().map(|node| self.file_row(node, cx)))
                        .into_any_element()
                })
            })
    }

    /// One row of the listing.
    ///
    /// A `Button` shaped as a row, not a `div` that happens to take clicks:
    /// opening a file is a control, and `Button` is the only thing here that
    /// tracks a focus handle, so it is the only thing that answers Enter and
    /// Space, joins the tab ring and draws a focus ring. `ListItem` looks the
    /// part but never calls `track_focus`, which leaves the row reachable by
    /// pointer only.
    ///
    /// The open file used to be marked by the lowercase word "open" alone,
    /// which is a caption where the reader is looking for a highlighted row.
    /// It now carries the selected background as well — the same one the tabs
    /// above use, because it is the same question: which of these am I in.
    fn file_row(&self, node: &FileNode, cx: &mut Context<Self>) -> AnyElement {
        let showing = self.is_open(&node.rel);
        let rel = node.rel.clone();
        // One step on the spacing scale per level: enough to read as nesting
        // without pushing a deep file off the pane.
        let indent = rems(0.5 + 0.75 * node.depth as f32);
        // A directory has nothing to open and a binary has nothing an editor
        // can do with it. Neither is a control, so neither takes a click, a
        // hover or a place in the tab ring — a stop that does nothing when the
        // user gets there is worse than no stop.
        let inert = node.is_dir || !node.editable;
        let selected = showing && !inert;

        Button::new(ElementId::from((
            ElementId::from("file-row"),
            node.rel.clone(),
        )))
        .ghost()
        .small()
        .w_full()
        .h_7()
        .pr_2()
        .pl(indent)
        .disabled(inert)
        .tab_stop(!inert)
        .selected(selected)
        // The button announces itself from this: its children are an icon and
        // a truncating div, and a listener needs the file named and told
        // whether it is the one already showing.
        .accessibility_label(if selected {
            format!("{}, open", node.label)
        } else {
            node.label.to_string()
        })
        // Ghost's disabled foreground is `muted_foreground` at half alpha,
        // which is fainter than a directory name should be. The instance
        // style is replayed inside the disabled state, so setting the colour
        // here restores the weight the row had before.
        .when(inert, |this| this.text_color(cx.theme().muted_foreground))
        .child(
            h_flex()
                .w_full()
                .min_w_0()
                .gap_2()
                .items_center()
                .child(
                    Icon::new(if node.is_dir {
                        IconName::Folder
                    } else {
                        IconName::FileText
                    })
                    .xsmall()
                    .flex_shrink_0()
                    .text_color(cx.theme().muted_foreground),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(node.label.clone()),
                )
                // Saying so beats a row that looks clickable and then refuses.
                .when(!node.is_dir && !node.editable, |this| {
                    this.child(row_note("not text", cx))
                })
                .when(selected, |this| this.child(row_note("open", cx))),
        )
        .when(!inert, |this| {
            this.on_click(
                cx.listener(move |this, _, window, cx| this.open_file(rel.clone(), window, cx)),
            )
        })
        .into_any_element()
    }

    // ------------------------------------------------------------------ tabs

    /// True when `rel` has a tab, whether or not it is the one showing.
    fn is_open(&self, rel: &SharedString) -> bool {
        rel == SKILL_FILE_NAME || self.open.iter().any(|file| &file.rel == rel)
    }

    /// Whether the tab now showing has edits that have not been written.
    ///
    /// Read from outside the pane: every way out of an edit — another skill,
    /// another scope, Cmd-W, Cmd-Q — asks this before it takes the work away.
    ///
    /// Not the same question as which tab wears a dot. The Overview answers
    /// for `SKILL.md` here even though it does not edit it, because it is the
    /// file that tab would save: a user who typed in the editor, came back to
    /// the Overview and then picked another skill has to be asked, or the work
    /// goes without a word.
    pub(crate) fn showing_dirty(&self) -> bool {
        match &self.showing {
            // Save on the Overview writes `SKILL.md`, so it is live exactly
            // when that file has something to write.
            Showing::Overview => self.source_edited,
            Showing::File(rel) if rel == SKILL_FILE_NAME => self.source_edited,
            Showing::File(rel) => self
                .open
                .iter()
                .find(|file| &file.rel == rel)
                .is_some_and(|file| file.dirty),
        }
    }

    /// The file the active tab writes, relative to the skill directory.
    fn showing_file(&self) -> SharedString {
        match &self.showing {
            // The Overview reads `SKILL.md`, so that is the file its Save
            // writes.
            Showing::Overview => SKILL_FILE_NAME.into(),
            Showing::File(rel) => rel.clone(),
        }
    }

    /// Ask what to do with the unsaved edits before something takes them away.
    ///
    /// Three answers, because all three are real: write the file, leave the
    /// edits behind, or stay. `proceed` runs for the first two, and for a
    /// failed write it does not: the notification says what went wrong and the
    /// user is still on the edit that could not be saved.
    ///
    /// `loss` completes the sentence that names what is about to happen —
    /// "Quitting discards them." — so the reader is told which of their
    /// gestures is the one that costs the work.
    pub(crate) fn confirm_discard(
        &mut self,
        loss: &'static str,
        proceed: Proceed,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.confirm_discard_file(self.showing_file(), loss, proceed, window, cx);
    }

    /// [`Self::confirm_discard`] for a named tab, which is not always the one
    /// showing: a tab's own close button can be aimed at any of them.
    fn confirm_discard_file(
        &mut self,
        file: SharedString,
        loss: &'static str,
        proceed: Proceed,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending = Some(proceed);

        let path = match &self.skill {
            Some(skill) => display_path(&skill.origin.join(file.as_ref()), &self.roots),
            None => file.clone(),
        };
        let this = cx.entity().downgrade();

        // The builder runs on every frame the dialog is up, so nothing that can
        // only happen once is captured here. The continuation waits on the pane
        // instead, and each button takes it from there.
        window.open_dialog(cx, move |dialog, _, _| {
            let this = this.clone();
            let body = format!("The edits in {path} have not been written to disk. {loss}");

            dialog
                .title(format!("Save changes to {file}?"))
                .width(px(440.))
                .content(move |content, _, _| {
                    content.child(div().p_4().text_sm().child(body.clone()))
                })
                .footer(
                    DialogFooter::new()
                        .p_4()
                        .child(
                            Button::new("discard-edits")
                                .danger()
                                .label("Discard")
                                .on_click({
                                    let this = this.clone();
                                    move |_, window, cx| {
                                        let proceed = this
                                            .update(cx, |this, _| this.pending.take())
                                            .ok()
                                            .flatten();
                                        window.close_dialog(cx);
                                        if let Some(proceed) = proceed {
                                            proceed(window, cx);
                                        }
                                    }
                                }),
                        )
                        .child(
                            Button::new("keep-editing")
                                .outline()
                                .label("Cancel")
                                .on_click({
                                    let this = this.clone();
                                    move |_, window, cx| {
                                        this.update(cx, |this, _| this.pending = None).ok();
                                        window.close_dialog(cx);
                                    }
                                }),
                        )
                        .child(Button::new("save-edits").primary().label("Save").on_click({
                            let this = this.clone();
                            let file = file.clone();
                            move |_, window, cx| {
                                let file = file.clone();
                                let proceed = this
                                    .update(cx, |this, _| this.pending.take())
                                    .ok()
                                    .flatten();
                                // This dialog goes first, and the save runs
                                // after it: `close_dialog` pops whichever
                                // dialog is on top, so a save that opens the
                                // rename confirmation would have that one
                                // popped instead of this one, and nothing
                                // would be written.
                                window.close_dialog(cx);
                                this.update(cx, |this, cx| {
                                    this.save_tab_then(file, proceed, window, cx);
                                })
                                .ok();
                            }
                        })),
                )
        });
    }

    /// Close the bundled file the pane is showing, when it is showing one.
    ///
    /// True when there was a tab to close, which is what makes Cmd-W close the
    /// file in front of the user before it closes the window. The Overview and
    /// `SKILL.md` are permanent tabs and are never what Cmd-W closes.
    pub(crate) fn close_showing_file(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Showing::File(rel) = self.showing.clone() else {
            return false;
        };
        if !self.open.iter().any(|file| file.rel == rel) {
            return false;
        }
        self.close_file(rel, window, cx);
        true
    }

    /// Save whatever the active tab holds.
    ///
    /// The Overview and the `SKILL.md` tab both save through [`Self::save_then`]:
    /// they are two views of one file, the Overview editing its frontmatter and
    /// the tab its text, and that one write reconciles them.
    ///
    /// Reachable from outside the pane: the header's Save button and the
    /// File > Save menu item are two entry points to this one commit, so the
    /// menu does not get a second path to disk.
    pub(crate) fn save_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_active_then(None, window, cx);
    }

    /// [`Self::save_active`], and then `then` if the write landed.
    fn save_active_then(
        &mut self,
        then: Option<Proceed>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_tab_then(self.showing_file(), then, window, cx);
    }

    /// Write one tab's file, whether or not it is the tab showing.
    ///
    /// `SKILL.md` goes through [`Self::save_then`] — the Overview and the
    /// `SKILL.md` tab are two views of that one file — and everything else is
    /// written as it stands.
    fn save_tab_then(
        &mut self,
        file: SharedString,
        then: Option<Proceed>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if file == SKILL_FILE_NAME {
            self.save_then(then, window, cx);
        } else {
            self.save_file(file, then, window, cx);
        }
    }

    /// Open a bundled file in a tab, or show the tab it already has.
    ///
    /// `SKILL.md` is never read again here: it has a tab from the moment the
    /// skill is selected, and re-reading it would throw away an unsaved edit.
    fn open_file(&mut self, rel: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        if rel == SKILL_FILE_NAME || self.open.iter().any(|file| file.rel == rel) {
            self.show_tab(Showing::File(rel), cx);
            return;
        }
        let Some(skill) = self.skill.clone() else {
            return;
        };

        let path = skill.origin.join(rel.as_ref());
        let state = cx.new(|cx| {
            let state = EditorState::new(window, cx);
            match language_for(&rel) {
                Some(language) => state.language(language),
                None => state,
            }
        });

        let key = rel.clone();
        let subscription = cx.subscribe(&state, move |this, _, event: &InputEvent, cx| {
            if !matches!(event, InputEvent::Change) {
                return;
            }
            if let Some(file) = this.open.iter_mut().find(|file| file.rel == key)
                && !file.dirty
            {
                file.dirty = true;
                cx.notify();
            }
        });

        self.open.push(OpenFile {
            rel: rel.clone(),
            path: path.clone(),
            state: state.clone(),
            dirty: false,
            error: None,
            _subscription: subscription,
        });
        self.show_tab(Showing::File(rel.clone()), cx);

        cx.spawn_in(window, async move |this, cx| {
            let read = cx
                .background_spawn(async move { fs::read_to_string(&path) })
                .await;
            this.update_in(cx, |this, window, cx| {
                let Some(index) = this.open.iter().position(|file| file.rel == rel) else {
                    // The tab was closed, or the selection moved to another
                    // skill, while the read was in flight.
                    return;
                };
                match read {
                    Ok(text) => {
                        this.open[index].dirty = false;
                        let state = this.open[index].state.clone();
                        // `set_value` emits no change event, so loading never
                        // marks the tab dirty.
                        state.update(cx, |state, cx| state.set_value(text, window, cx));
                    }
                    Err(error) => this.open[index].error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Close a bundled file's tab.
    ///
    /// An unsaved edit is confirmed first, because a tab's close button is a
    /// small target next to the label and hitting it by accident should not
    /// cost the user their work. The confirmation offers to write the file, so
    /// the accident costs nothing at all.
    fn close_file(&mut self, rel: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = self
            .open
            .iter()
            .find(|file| file.rel == rel)
            .is_some_and(|file| file.dirty);
        if !dirty {
            self.drop_file(&rel, cx);
            return;
        }

        // The same three answers as every other way out of an edit, so a tab's
        // close button costs no more than the wrong click it usually is.
        let this = cx.entity().downgrade();
        let key = rel.clone();
        self.confirm_discard_file(
            rel,
            "Closing the tab discards them.",
            Box::new(move |_, cx| {
                this.update(cx, |this, cx| this.drop_file(&key, cx)).ok();
            }),
            window,
            cx,
        );
    }

    fn drop_file(&mut self, rel: &SharedString, cx: &mut Context<Self>) {
        self.open.retain(|file| &file.rel != rel);
        if self.showing == Showing::File(rel.clone()) {
            // Back to the Overview rather than to a neighbouring tab: which
            // neighbour is arbitrary, and the Overview is where the file list
            // is, so it is where the user goes next either way.
            self.showing = Showing::Overview;
        }
        // Every tab to the right of the one that closed has moved a place, so
        // the one still showing is put back in view.
        self.tab_scroll
            .scroll_to_item(self.tab_index(&self.showing));
        cx.notify();
    }

    /// Write the bundled file showing in the active tab back to disk.
    ///
    /// Nothing parses it and nothing rewrites it. A bundled file is whatever
    /// the skill's author put there, so what the editor holds is what is
    /// written.
    fn save_file(
        &mut self,
        rel: SharedString,
        then: Option<Proceed>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self.open.iter().find(|file| file.rel == rel) else {
            return;
        };
        if self.busy {
            return;
        }
        let path = file.path.clone();
        let text = file.state.read(cx).value().to_string();

        self.busy = true;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let written = cx
                .background_spawn({
                    let path = path.clone();
                    let text = text.clone();
                    async move { fs::write(&path, text) }
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match written {
                    Ok(()) => {
                        if let Some(file) = this.open.iter_mut().find(|file| file.rel == rel) {
                            file.dirty = false;
                            file.error = None;
                        }
                        window.push_notification(
                            Notification::success(format!(
                                "Wrote {}",
                                display_path(&path, &this.roots)
                            ))
                            .title("Saved"),
                            cx,
                        );
                        run_after(then, window, cx);
                    }
                    Err(error) => window.push_notification(
                        Notification::error(error.to_string()).title("Could not save"),
                        cx,
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The pane's tab strip: the Overview, `SKILL.md`, and whatever else the
    /// user has opened.
    ///
    /// `SKILL.md` has a permanent tab because every skill has that file and
    /// editing it is what the pane is mostly for. The rest are opened from the
    /// Overview's file list and can be closed again.
    ///
    /// A row of ghost buttons rather than a `TabBar`. All this band has to say
    /// is which tab is current, and a ghost button already says exactly that in
    /// the same language as every other control in the pane: transparent until
    /// it is hovered or selected, and a grey surface when it is. Every `TabBar`
    /// variant insists on more than that — a trough behind the row, or a rule
    /// under it — which would fight the drag band the strip now sits inside
    /// instead of reading as part of it.
    fn tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // The dot marks where the unsaved work was done, not every tab that
        // would write it. The Overview reads `SKILL.md` rather than editing
        // it, so a dot there would point at a tab with nothing to fix in it.
        let skill_file = Showing::File(SKILL_FILE_NAME.into());
        let mut labels: Vec<(Showing, SharedString, bool, bool)> = vec![
            (
                Showing::Overview,
                "Overview".into(),
                Showing::Overview.dot(self.source_edited),
                false,
            ),
            (
                skill_file.clone(),
                SKILL_FILE_NAME.into(),
                skill_file.dot(self.source_edited),
                false,
            ),
        ];
        labels.extend(self.open.iter().map(|file| {
            (
                Showing::File(file.rel.clone()),
                self.tab_label(&file.rel),
                file.dirty,
                true,
            )
        }));

        h_flex()
            // Enough open files to overrun the pane scroll rather than being
            // clipped off the right edge. The row carries no visible scrollbar,
            // because a bar under a 24-pixel strip is thicker than the thing it
            // measures; the tabs that are cut off are their own affordance.
            .id("detail-tabs")
            .track_scroll(&self.tab_scroll)
            .flex_1()
            .min_w_0()
            .overflow_x_scroll()
            .gap_1()
            .children(labels.into_iter().map(|(showing, label, dirty, closable)| {
                self.tab(showing, label, dirty, closable, cx)
            }))
    }

    /// One tab in the strip.
    fn tab(
        &self,
        showing: Showing,
        label: SharedString,
        dirty: bool,
        closable: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = match &showing {
            Showing::Overview => ElementId::from("detail-tab-overview"),
            Showing::File(rel) => ElementId::from((ElementId::from("detail-tab"), rel.clone())),
        };
        let selected = self.showing == showing;
        let target = showing.clone();

        Button::new(id)
            .ghost()
            .small()
            .selected(selected)
            .label(label)
            .child(
                // A lane the dot sits in whether or not it is showing, so a tab
                // does not resize the moment the user types and the strip does
                // not shuffle under the pointer.
                div().flex_shrink_0().size_1p5().when(dirty, |this| {
                    this.rounded_full().bg(cx.theme().muted_foreground)
                }),
            )
            .when(closable, |this| {
                let Showing::File(rel) = showing else {
                    return this;
                };
                // A Button, not a bare icon in a zero-padding div: the xsmall
                // geometry gives the 12-pixel glyph a hit area to sit in, and
                // brings the focus handle a keyboard needs to reach it at all.
                this.child(
                    Button::new(ElementId::from((ElementId::from("close-tab"), rel.clone())))
                        .ghost()
                        .xsmall()
                        .flex_shrink_0()
                        .icon(IconName::Close)
                        .accessibility_label(format!("Close {rel}"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            // The close button sits inside the tab's own
                            // button, so without this the tab would also read
                            // the click and select the tab on its way out.
                            cx.stop_propagation();
                            this.close_file(rel.clone(), window, cx);
                        })),
                )
            })
            .on_click(cx.listener(move |this, _, _, cx| this.show_tab(target.clone(), cx)))
            .into_any_element()
    }

    /// Show `showing`, bringing its tab into view.
    ///
    /// A file opened from the Overview's list gets its tab at the end of a
    /// strip that may already be wider than the pane, and a selected tab the
    /// user cannot see reads as nothing having happened.
    fn show_tab(&mut self, showing: Showing, cx: &mut Context<Self>) {
        self.tab_scroll.scroll_to_item(self.tab_index(&showing));
        self.showing = showing;
        cx.notify();
    }

    /// Where a tab sits in the strip. The Overview leads, `SKILL.md` follows,
    /// and the opened files come after in the order they were opened.
    fn tab_index(&self, showing: &Showing) -> usize {
        match showing {
            Showing::Overview => 0,
            Showing::File(rel) if rel == SKILL_FILE_NAME => 1,
            Showing::File(rel) => self
                .open
                .iter()
                .position(|file| &file.rel == rel)
                .map_or(0, |index| index + 2),
        }
    }

    /// What a file's tab is called.
    ///
    /// The file name alone, because `references/browse-the-web.md` across a tab
    /// leaves room for nothing else. The directory comes back only when two
    /// open files would otherwise carry the same label, which is the one case
    /// where the short form does not identify the file.
    fn tab_label(&self, rel: &SharedString) -> SharedString {
        let name = Path::new(rel.as_ref())
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| rel.to_string());

        let ambiguous = self
            .open
            .iter()
            .filter(|other| &other.rel != rel)
            .any(|other| {
                Path::new(other.rel.as_ref())
                    .file_name()
                    .is_some_and(|other| other.to_string_lossy() == name)
            });

        if ambiguous {
            rel.clone()
        } else {
            SharedString::from(name)
        }
    }

    /// The editor for whichever file the active tab holds.
    fn file_tab(&self, rel: &SharedString, cx: &mut Context<Self>) -> AnyElement {
        if rel == SKILL_FILE_NAME {
            return div()
                .size_full()
                .child(Editor::new(&self.body).h_full())
                .into_any_element();
        }

        let Some(file) = self.open.iter().find(|file| &file.rel == rel) else {
            // The tab was closed underneath us; the next frame drops it.
            return div().into_any_element();
        };

        if let Some(error) = &file.error {
            return v_flex()
                .p_5()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(error.clone()),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(display_path(&file.path, &self.roots)),
                )
                .into_any_element();
        }

        div()
            .size_full()
            .child(Editor::new(&file.state).h_full())
            .into_any_element()
    }
}

/// The agents a bulk button is about to act on, named.
fn names_of(agents: &[&'static AgentDef]) -> String {
    let names: Vec<String> = agents
        .iter()
        .map(|agent| agent.display_name.to_string())
        .collect();
    in_a_list(&names)
}

/// Why an agent the user came looking for has no row.
///
/// The list is filtered to the agents on this machine, which is right — a
/// switch for something that is not installed writes a link nothing reads —
/// but a filter that removes six of fourteen rows without a word reads as a
/// missing feature rather than a decision.
///
/// The names, not a count of them. The sidebar counts agents and this section
/// lists only the ones that can hold a link of their own, so two numbers on
/// one screen would differ by the agents covered by Shared and look like a
/// contradiction. The names answer the question either way.
fn absent_sentence(names: &[&'static str]) -> String {
    match names {
        [] => String::new(),
        [one] => format!("{one} is not installed on this machine, so it has no row here."),
        _ => format!(
            "These agents are not installed on this machine, so they have no rows here: {}.",
            names.join(", ")
        ),
    }
}

/// What turning an agent's Enabled switch off actually does.
/// Join phrases the way a sentence does: "a", "a and b", "a, b, and c".
fn in_a_list(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [one] => one.clone(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

/// Run what was waiting on a save, once the pane has finished updating.
///
/// A save's continuation lands in the middle of the pane's own update, and
/// everything that waits on one wants the pane back: another skill in it, a tab
/// closed, the window gone. Deferring runs it once this update has finished,
/// with nothing borrowed.
fn run_after(then: Option<Proceed>, window: &mut Window, cx: &mut App) {
    if let Some(then) = then {
        window.defer(cx, move |window, cx| then(window, cx));
    }
}

/// The repository and ref a provenance names, when it names one that can be
/// read. A provenance whose `repo_url` is not a GitHub repository has nothing
/// to ask about, which is exactly what `UpdateStatus::Unknown` reports.
fn repo_ref_of(provenance: &Provenance) -> Option<RepoRef> {
    let (owner, repo) = provenance.owner_repo()?;
    Some(RepoRef::new(owner, repo, &provenance.reference))
}

/// Where a provenance says the skill can be downloaded from again.
fn location_of(provenance: &Provenance) -> Option<SkillLocation> {
    Some(SkillLocation::new(
        repo_ref_of(provenance)?,
        &provenance.path,
    ))
}

fn disable_effect(agent: &AgentDef, roots: &Roots) -> String {
    match agent.disable {
        DisableMode::MoveAside(dir) => format!(
            "Off parks the link in {}, where {} will not read it.",
            display_path(&roots.home().join(dir), roots),
            agent.display_name
        ),
        DisableMode::CodexConfig => format!(
            "Off sets enabled = false in {}, leaving the link where it is.",
            display_path(&roots.codex_config(), roots)
        ),
        DisableMode::RemoveLink => format!(
            "{} has no separate off state; removing the link is the only way.",
            agent.display_name
        ),
    }
}

/// What to call the scope a duplicate directory sits in.
///
/// A duplicate found through `conflicts` rather than through a location has no
/// agent, which is worth saying rather than papering over.
fn scope_label(agent_id: &str) -> &'static str {
    match agent_id {
        PRIVATE_ID => "Hidden skills",
        "" => "Unknown scope",
        id => agent_label(id),
    }
}

/// Who can see this skill, in as few words as the header has room for.
///
/// Named rather than counted where the names fit: "Claude Code, Codex" tells
/// the reader whether to open the section, and "2 agents" does not. This is
/// the information the list rows used to carry as a row of tags under every
/// skill, which was three lines of chrome per row for a question the reader
/// was rarely asking at that moment.
///
/// Only agents on this machine, `installed`, which is the filter
/// [`SkillView::reach`] applies. `visible_to` on its own answers "would this
/// agent reach the skill", which is true of every agent that reads the shared
/// directory whether or not it is here, and naming one of those put it in this
/// line and in the "not installed" sentence two rows below at the same time.
fn reach(
    skill: &SkillView,
    installed: &[&'static AgentDef],
    cx: &mut Context<DetailPane>,
) -> SharedString {
    const NAMED: usize = 3;

    let mut names: Vec<&'static str> = Vec::new();
    if skill.in_shared {
        names.push("Shared");
    }
    names.extend(
        skill
            .reach(installed)
            .into_iter()
            .map(|agent| agent.display_name),
    );

    let _ = cx;
    match names.len() {
        0 => "No agent can see this".into(),
        n if n <= NAMED => names.join(", ").into(),
        n => format!("{}, +{}", names[..NAMED].join(", "), n - NAMED).into(),
    }
}

/// The greyed word at the end of a file row, for what the row cannot say by
/// looking like itself: a file the editor will not take, or the one showing.
// `use<>`: the element owns everything it needs, so it must not be tied to the
// borrow of `App` the colour was read through.
fn row_note(text: &'static str, cx: &App) -> impl IntoElement + use<> {
    div()
        .flex_shrink_0()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

/// A `?` a reader can hover for the sentence a row would otherwise carry under
/// it.
///
/// The rows in "Visible to" each used to sit above a paragraph explaining
/// them, which made a dozen switches into a page of prose. What a row's state
/// means is worth keeping and is worth reading once, so it goes here: a glyph
/// the width of the switch labels beside it, and the sentence a hover away.
// `use<>`: the element owns everything it needs, so it must not be tied to the
// borrow of `App` the colours were read through.
fn help_dot(id: ElementId, text: String, cx: &App) -> impl IntoElement + use<> {
    h_flex()
        .id(id)
        .flex_shrink_0()
        .size_4()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(cx.theme().muted)
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child("?")
        .tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
}

/// One agent's share of linking or unlinking, appended to `done`.
///
/// Takes the outcome to append to rather than returning one of its own, so
/// that a refusal leaves what already happened in the caller's hands. Switching
/// an agent off unparks its link before it can unlink it: if the unlink is what
/// fails, the link has still been moved out of the agent's disabled directory,
/// and only the caller holds that fact to put in the message.
///
/// Shared by the one-switch and the whole-set paths, so the two cannot come to
/// disagree about what switching an agent off does.
fn set_present_step(
    installer: &Installer,
    done: &mut Outcome,
    name: &str,
    origin: &Path,
    agent: &'static AgentDef,
    on: bool,
    parked: bool,
) -> Result<(), InstallError> {
    if on {
        done.changes
            .extend(installer.link(name, origin, agent)?.changes);
        return Ok(());
    }
    // A link parked in the agent's disabled directory is not where `unlink`
    // looks, so there is nothing to unlink until it is moved back.
    if parked {
        done.changes
            .extend(installer.enable(name, origin, agent)?.changes);
    }
    done.changes.extend(installer.unlink(name, agent)?.changes);
    Ok(())
}

/// The page on github.com the update sentence links at, and the label that
/// says what is on it.
///
/// "An update is available. main holds abc1234 now" describes the whole
/// difference in seven hex characters, and the confirmation that follows
/// describes only what happens to the local directory. This is the other side
/// of it, on the repository that holds it.
///
/// Two shapes, because only one of them is a comparison:
///
/// - `/compare/{installed commit}...{ref}` when the install recorded the commit
///   its ref pointed at. GitHub resolves a branch, a tag or a commit there and
///   nothing else — a *tree* sha answers 404, which is why the tree sha the
///   update check compares cannot be either side of it. The head is the ref
///   rather than a recorded commit, so the link still means "since this was
///   installed" however stale the last check is.
/// - `/tree/{ref}/{path}` otherwise: the directory as it stands upstream now.
///   Every skill installed before the commit was recorded lands here, as does
///   every skill `npx skills` installed. It is not a difference, and the label
///   does not call it one.
fn upstream_for(slug: &str, reference: &str, path: &str, installed_commit: &str) -> Upstream {
    if installed_commit.is_empty() {
        let path = path.trim_matches('/');
        let url = if path.is_empty() {
            format!("https://github.com/{slug}/tree/{reference}")
        } else {
            format!("https://github.com/{slug}/tree/{reference}/{path}")
        };
        return Upstream {
            url: url.into(),
            label: "See what is there now",
        };
    }
    Upstream {
        url: format!("https://github.com/{slug}/compare/{installed_commit}...{reference}").into(),
        label: "See what changed",
    }
}

/// The link out to GitHub, built once for the two places that show it — the
/// Source section and the update confirmation — so the label and the URL
/// cannot drift apart between them.
///
/// A `Link`, not a Button: it leaves the application for github.com, which is
/// the one thing underlining is for.
fn upstream_element(id: &'static str, upstream: Upstream) -> impl IntoElement {
    Link::new(id).href(upstream.url).text_sm().child(
        h_flex()
            .gap_1()
            .items_center()
            .child(upstream.label)
            .child(Icon::new(IconName::ExternalLink).xsmall()),
    )
}

fn section_title(label: &'static str, cx: &mut Context<DetailPane>) -> impl IntoElement {
    // A step smaller than the body it heads, so weight rather than size is what
    // separates the two: same colour, same scale step, heavier.
    div()
        .text_xs()
        .font_medium()
        .text_color(cx.theme().muted_foreground)
        .child(label)
}

/// Whether a listing of `rows` rows opens with the skill.
///
/// A skill with three files is answered by its listing, so it is shown. A skill
/// with thirty is not: the listing is then the whole of the Overview, and the
/// sections under it — Location, Visible to, Source — are off the screen. The
/// heading says how many there are, so nothing is hidden without being counted.
fn files_open_by_default(rows: usize) -> bool {
    rows <= MAX_ROWS_OPEN
}

/// What the Files heading says beside itself, so a closed listing still reports
/// the size of the directory.
///
/// Folders are counted separately from files: they are rows of the listing but
/// not things to open, and "30 files" for a directory holding 24 would be
/// wrong. Nothing at all for a directory that could not be read — the line
/// under the heading says that instead.
fn files_summary(tree: &[FileNode]) -> Option<String> {
    if tree.is_empty() {
        return None;
    }
    let dirs = tree.iter().filter(|node| node.is_dir).count();
    let files = tree.len() - dirs;
    let files = match files {
        1 => "1 file".to_string(),
        n => format!("{n} files"),
    };
    Some(match dirs {
        0 => files,
        1 => format!("{files} in 1 folder"),
        n => format!("{files} in {n} folders"),
    })
}

/// The top-level entries beside `SKILL.md`, directories marked with a slash.
/// List a skill directory depth-first, `SKILL.md` first and the rest sorted
/// with directories before files.
fn list_tree(dir: &Path) -> Vec<FileNode> {
    let mut out = Vec::new();
    walk(dir, "", 0, &mut out);

    // `SKILL.md` is the file the user came for, so it leads regardless of
    // where sorting would otherwise put it.
    if let Some(index) = out
        .iter()
        .position(|node| node.depth == 0 && node.rel == SKILL_FILE_NAME)
    {
        let skill_file = out.remove(index);
        out.insert(0, skill_file);
    }
    out
}

fn walk(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<FileNode>) {
    if depth >= MAX_TREE_DEPTH || out.len() >= MAX_TREE_ENTRIES {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    let mut listed: Vec<(String, bool)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            // A dotfile in a skill directory belongs to an editor or to version
            // control, not to the skill.
            if name.starts_with('.') {
                return None;
            }
            let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
            Some((name, is_dir))
        })
        .collect();
    // Directories first, then alphabetical, which is what a file manager shows
    // and therefore what the user is expecting.
    listed.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    for (name, is_dir) in listed {
        if out.len() >= MAX_TREE_ENTRIES {
            return;
        }
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        out.push(FileNode {
            rel: SharedString::from(rel.clone()),
            label: SharedString::from(name.clone()),
            depth,
            is_dir,
            editable: !is_dir && !is_binary(&name),
        });
        if is_dir {
            walk(&dir.join(&name), &rel, depth + 1, out);
        }
    }
}

fn is_binary(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .is_some_and(|ext| BINARY_EXTENSIONS.contains(&ext.as_str()))
}

/// The editor language for a file, by extension.
///
/// The list is short because it names only the grammars this binary actually
/// compiles in — the `tree-sitter-*` features in `Cargo.toml`. Naming one that
/// is not there would not fail, it would silently produce plain text, so the
/// two lists are kept in step deliberately.
///
/// `None` means no highlighting, which is also the right answer for a file
/// whose type the editor does not know: plain text stays readable, and the
/// wrong grammar is worse than none.
fn language_for(rel: &str) -> Option<&'static str> {
    let ext = Path::new(rel)
        .extension()
        .and_then(|ext| ext.to_str())?
        .to_ascii_lowercase();
    Some(match ext.as_str() {
        "md" | "markdown" | "mdx" => "markdown",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "sh" | "bash" | "zsh" => "bash",
        "py" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        _ => return None,
    })
}

/// Reveal a directory in the platform's file manager.
///
/// `open -R` selects the directory in its parent window, which is what "Reveal
/// in Finder" means on macOS; plain `open` on a directory opens the directory
/// itself, which is a different thing and not what the button says. Linux has
/// no portable equivalent, so `xdg-open` opens the directory there.
fn open_in_file_manager(dir: &Path) -> Result<(), String> {
    let (program, reveal) = if cfg!(target_os = "macos") {
        ("open", true)
    } else {
        ("xdg-open", false)
    };
    let mut command = std::process::Command::new(program);
    if reveal {
        command.arg("-R");
    }
    command
        .arg(dir)
        .status()
        .map_err(|e| format!("{program}: {e}"))
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(format!("{program} exited with {status}"))
            }
        })
}

/// Hand a skill directory to the user's own editor.
///
/// `$VISUAL` first, then `$EDITOR`, both of which may carry arguments —
/// `code -n`, `zed --wait` — so the first word is the program and the rest are
/// passed through. With neither set there is nothing to prefer, so the
/// directory goes to whatever the platform opens a folder with.
///
/// Spawned rather than waited on: an editor asked to wait does not return
/// until the user closes the window, and a status this pane will never see is
/// worse than none. A program that is not on `PATH` still fails here, which is
/// the failure worth reporting.
fn open_in_editor(dir: &Path) -> Result<(), String> {
    let configured = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .ok()
        .filter(|value| !value.trim().is_empty());

    let mut command = match &configured {
        Some(value) => {
            let mut words = value.split_whitespace();
            // `filter` above rules out an all-whitespace value, so there is a
            // first word.
            let program = words.next().unwrap_or(value);
            let mut command = std::process::Command::new(program);
            command.args(words);
            command
        }
        None if cfg!(target_os = "macos") => std::process::Command::new("open"),
        None => std::process::Command::new("xdg-open"),
    };

    command
        .arg(dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| match &configured {
            Some(value) => format!("{value}: {e}"),
            None => format!("{e}. Set $EDITOR or $VISUAL to choose an editor."),
        })
}

/// The validation findings for one frontmatter field.
fn issues_for<'a>(issues: &'a [Issue], field: &str) -> Vec<&'a Issue> {
    issues
        .iter()
        .filter(|issue| issue.field == Some(field))
        .collect()
}

fn issue_lines(issues: Vec<&Issue>, cx: &mut Context<DetailPane>) -> Vec<AnyElement> {
    issues
        .into_iter()
        .map(|issue| {
            div()
                .text_xs()
                .text_color(if issue.is_error {
                    cx.theme().danger
                } else {
                    cx.theme().warning
                })
                .child(issue.message.clone())
                .into_any_element()
        })
        .collect()
}

impl Render for DetailPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(skill) = self.skill.clone() else {
            return self.empty_state(window, cx).into_any_element();
        };

        let body = match self.showing.clone() {
            Showing::Overview => self.overview(&skill, cx),
            // A file tab is the editor and nothing else: it fills the pane and
            // owns its own scrolling, so the file scrolls rather than the pane
            // growing to fit it.
            Showing::File(rel) => self.file_tab(&rel, cx),
        };

        v_flex()
            .size_full()
            .min_w_0()
            .bg(cx.theme().background)
            .child(self.header(&skill, window, cx))
            .child(self.identity_row(&skill, cx))
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }
}

impl DetailPane {
    /// What the skill's frontmatter says it is: the name it gives itself and
    /// the description an agent matches against.
    ///
    /// Read out, not edited. Both are lines in `SKILL.md`, and the `SKILL.md`
    /// tab is where a line in `SKILL.md` is changed — a second control on the
    /// same two keys meant the file could be edited from two places at once
    /// and one of them had to win.
    ///
    /// Renaming is the exception, and it is not an edit to this text: the name
    /// is the directory's name too, so it moves a folder. It has its own
    /// action in the band above.
    ///
    /// Both go quiet for a skill whose frontmatter does not parse: there is
    /// nothing to read out of it. The band above says so and points at the tab
    /// that fixes it.
    fn summary(&self, skill: &SkillView, cx: &mut Context<Self>) -> impl IntoElement {
        // An empty value is a fact about the file, not a blank: a skill with
        // no description is one an agent has nothing to match against.
        let muted = cx.theme().muted_foreground;

        v_flex()
            .flex_shrink_0()
            .gap_4()
            .max_w(px(PROSE_MAX_WIDTH))
            .child(
                v_flex()
                    .gap_1()
                    .child(section_title("Name", cx))
                    .child(if self.loaded_name.is_empty() {
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child("No name in the frontmatter")
                    } else {
                        div().text_sm().child(self.loaded_name.clone())
                    })
                    .children(issue_lines(issues_for(&skill.issues, "name"), cx)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(section_title("Description", cx))
                    .child(if self.loaded_description.is_empty() {
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child("No description in the frontmatter")
                    } else {
                        div().text_sm().child(self.loaded_description.clone())
                    })
                    .children(issue_lines(issues_for(&skill.issues, "description"), cx)),
            )
    }

    /// The first tab: what the skill is, what is in its directory, and who can
    /// see it.
    ///
    /// The file itself is not here. It used to be, under a heading at the
    /// bottom of a column that had to be scrolled past the description, the
    /// switches and the duplicate list to reach. It now has a tab of its own.
    fn overview(&self, skill: &SkillView, cx: &mut Context<Self>) -> AnyElement {
        let read_error = match &self.source {
            Source::Failed(error) => Some(error.clone()),
            _ => None,
        };

        v_flex()
            // One scroll owner for the tab.
            .id("detail-overview")
            .size_full()
            // With a bar: the tab runs past the fold — the agent list alone is
            // cut mid-row — and nothing else says there is more below.
            .overflow_y_scrollbar()
            .px_5()
            .py_5()
            .gap_6()
            .when_some(read_error, |this, error| {
                this.child(
                    div()
                        .p_3()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().danger.opacity(0.1))
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
            .when_some(skill.parse_error.clone(), |this, error| {
                // A skill that does not parse still opens, so that the SKILL.md
                // tab is where it gets fixed.
                this.child(
                    v_flex()
                        .p_3()
                        .gap_1()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().warning.opacity(0.12))
                        .child(div().text_sm().text_color(cx.theme().warning).child(error))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(
                                    "The name and description below stay empty until the \
                                     frontmatter parses. Fix it in the SKILL.md tab and save.",
                                ),
                        ),
                )
            })
            .child(self.summary(skill, cx))
            // Location before Visibility: the switches below are read-only
            // until an unmanaged skill is adopted, and Adopt lives in
            // Location. Ownership answered first, then what it allows.
            .child(self.location(skill, cx))
            .child(self.visibility(skill, cx))
            // The listing comes after both, because it is the one section
            // whose height is the skill's rather than the pane's: thirty rows
            // for `docx`, one for a skill that is a single file. Above them it
            // decided how far down "Visible to" — the answer to "can my agent
            // use this yet" — began. Below them the answer is always in the
            // same place, and the listing can be as long as the directory is.
            .child(self.folder_structure(cx))
            .child(self.duplicates_section(cx))
            .child(self.source_section(skill, cx))
            .into_any_element()
    }
}

/// The sentence above the duplicate rows, with the counts written out.
///
/// Three phrasings rather than one template: "0 have diverged and are left
/// alone" is a sentence about nothing, and a reader has to stop and work out
/// that it means everything is fine.
fn duplicates_summary(
    copies: usize,
    identical: usize,
    differing: usize,
    origin: &SharedString,
) -> String {
    let opening = format!(
        "{copies} separate director{} {} this skill besides the origin.",
        if copies == 1 { "y" } else { "ies" },
        if copies == 1 { "holds" } else { "hold" },
    );
    let rest = if differing == 0 {
        format!(
            " {} {origin} exactly.",
            if identical == 1 {
                "It matches"
            } else {
                "They all match"
            }
        )
    } else if identical == 0 {
        format!(
            " {} diverged from {origin}, so {} left alone unless you say otherwise.",
            if differing == 1 {
                "It has"
            } else {
                "They have all"
            },
            if differing == 1 { "it is" } else { "they are" },
        )
    } else {
        format!(
            " {identical} {} {origin} exactly; {differing} {} diverged and {} left alone unless \
             you say otherwise.",
            if identical == 1 { "matches" } else { "match" },
            if differing == 1 { "has" } else { "have" },
            if differing == 1 { "is" } else { "are" },
        )
    };
    opening + &rest
}

/// Why `typed` cannot replace the skill's current name, or `None` when it can.
///
/// The name is the directory's name as well as the frontmatter's, so this
/// answers for both: the rules creation applies, plus the two a rename raises
/// on its own — a name cannot be emptied, and the directory it would move to
/// has to be free. The scan answers the first collision and the filesystem the
/// second, because a directory can sit beside the store without holding a skill
/// the scan would list.
fn rename_problem(
    typed: &str,
    skill: &SkillView,
    scan: Option<&Scan>,
    roots: &Roots,
) -> Option<SharedString> {
    name_rules(typed, skill, scan).or_else(|| destination_taken(typed, skill, roots))
}

/// The half of [`rename_problem`] that only reads memory: the rules creation
/// applies, plus the one a rename raises on its own — a name cannot be emptied.
///
/// Split out because the other half is a syscall, and the two are asked
/// separately when only one of them can have changed.
fn name_rules(typed: &str, skill: &SkillView, scan: Option<&Scan>) -> Option<SharedString> {
    if typed == skill.name.as_ref() {
        return None;
    }
    if typed.is_empty() {
        return Some("A skill needs a name. It names the directory as well.".into());
    }
    if typed.chars().count() > MAX_NAME_LEN {
        return Some(format!("Too long. A name is at most {MAX_NAME_LEN} characters.").into());
    }
    if !is_kebab_case(typed) {
        return Some(
            "Use lowercase letters, digits, and single hyphens between them: my-new-skill.".into(),
        );
    }
    if scan.is_some_and(|scan| scan.get(typed).is_some()) {
        return Some(format!("“{typed}” is already a skill on this machine.").into());
    }
    None
}

/// The half of [`rename_problem`] that reads the disk: the directory the skill
/// would move to has to be free.
///
/// The scan cannot answer this, because a directory can sit beside the store
/// without holding a skill the scan would list. Call it when the name changes,
/// not while rendering: [`RenameCheck`] holds the answer in between.
fn destination_taken(typed: &str, skill: &SkillView, roots: &Roots) -> Option<SharedString> {
    if typed == skill.name.as_ref() {
        return None;
    }
    let destination = skill.origin.with_file_name(typed);
    destination.symlink_metadata().is_ok().then(|| {
        format!(
            "{} is already there. Rename or remove it, then try again.",
            display_path(&destination, roots)
        )
        .into()
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use skillbase_core::Roots;

    use super::super::model::{Scan, SkillView};
    use super::{
        FileNode, MAX_ROWS_OPEN, SKILL_FILE_NAME, Showing, absent_sentence, destination_taken,
        files_open_by_default, files_summary, fs, in_a_list, name_rules, rename_problem,
        upstream_for,
    };

    /// A directory of this test's own, named so two runs cannot collide.
    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "skillbase-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    fn skill_named(name: &str, origin: PathBuf) -> SkillView {
        SkillView {
            name: name.to_string().into(),
            description: "".into(),
            origin,
            managed: true,
            parse_error: None,
            issues: Vec::new(),
            conflicts: Vec::new(),
            locations: Vec::new(),
            codex_disabled: false,
            visible_to: Vec::new(),
            in_shared: false,
            provenance: None,
        }
    }

    #[test]
    fn a_rename_is_refused_for_the_reasons_a_new_name_is_refused() {
        let home = scratch("rename-rules");
        let roots = Roots::new(home.clone());
        let store = roots.store_dir();
        fs::create_dir_all(&store).expect("a store directory");
        let skill = skill_named("pdf", store.join("pdf"));

        // The name it already has: nothing to say, and nothing to move.
        assert_eq!(rename_problem("pdf", &skill, None, &roots), None);
        // A rename to nothing would leave the directory unnameable, which the
        // create dialog never has to answer because it disables its own button.
        assert!(
            rename_problem("", &skill, None, &roots)
                .expect("an empty name is refused")
                .contains("needs a name")
        );
        for bad in ["My Skill", "my_skill", "double--hyphen", "trail-"] {
            let said = rename_problem(bad, &skill, None, &roots)
                .unwrap_or_else(|| panic!("{bad} should be refused"));
            assert!(said.contains("my-new-skill"), "{bad}: {said}");
        }

        let taken = Scan {
            skills: vec![skill_named("notes", store.join("notes"))],
            ..Scan::default()
        };
        assert!(
            rename_problem("notes", &skill, Some(&taken), &roots)
                .expect("a name the scan already holds is refused")
                .contains("already a skill")
        );
        assert_eq!(rename_problem("notes", &skill, None, &roots), None);
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_rename_onto_an_occupied_directory_is_refused_even_when_the_scan_lists_nothing() {
        // A directory can sit beside the store without holding a skill the scan
        // would list — an empty one, or one with no SKILL.md. The move would
        // still have nowhere to land, so the filesystem gets the last word.
        let home = scratch("rename-occupied");
        let roots = Roots::new(home.clone());
        let store = roots.store_dir();
        fs::create_dir_all(store.join("notes")).expect("an occupied destination");
        let skill = skill_named("pdf", store.join("pdf"));

        assert!(
            rename_problem("notes", &skill, None, &roots)
                .expect("an occupied destination is refused")
                .contains("already there")
        );
        assert_eq!(rename_problem("charts", &skill, None, &roots), None);
        fs::remove_dir_all(&home).ok();
    }

    /// The pane runs the rules on every frame and the disk check only when the
    /// Name field changes, so the two have to be separable: the rules have to
    /// pass a name the disk refuses.
    #[test]
    fn the_rules_pass_a_name_only_the_disk_refuses() {
        let home = scratch("rename-split");
        let roots = Roots::new(home.clone());
        let store = roots.store_dir();
        fs::create_dir_all(store.join("notes")).expect("an occupied destination");
        let skill = skill_named("pdf", store.join("pdf"));

        assert_eq!(name_rules("notes", &skill, None), None);
        assert!(
            destination_taken("notes", &skill, &roots)
                .expect("an occupied destination is refused")
                .contains("already there")
        );
        // The name it already has moves nothing, so neither half has anything
        // to say and the disk is never asked.
        assert_eq!(name_rules("pdf", &skill, None), None);
        assert_eq!(destination_taken("pdf", &skill, &roots), None);
        fs::remove_dir_all(&home).ok();
    }

    /// The shapes are checked against github.com, not inferred: `/compare`
    /// resolves a branch, a tag or a commit and answers 404 for a tree sha,
    /// which is what the link used to hand it.
    #[test]
    fn an_install_commit_gets_a_comparison_and_nothing_else_gets_one() {
        let with_commit = upstream_for("o/r", "main", "skills/pdf", "c0ffee");
        assert_eq!(
            with_commit.url,
            "https://github.com/o/r/compare/c0ffee...main"
        );
        assert_eq!(with_commit.label, "See what changed");

        // Installed before the commit was recorded, or installed by something
        // that records none. The directory upstream is all there is to show,
        // and the label says so rather than promising a difference.
        let without = upstream_for("o/r", "main", "skills/pdf", "");
        assert_eq!(without.url, "https://github.com/o/r/tree/main/skills/pdf");
        assert_eq!(without.label, "See what is there now");

        // A skill that is the whole repository has no path segment to append,
        // and a trailing slash would be one.
        assert_eq!(
            upstream_for("o/r", "v2", "", "").url,
            "https://github.com/o/r/tree/v2"
        );
        assert_eq!(
            upstream_for("o/r", "main", "/skills/pdf/", "").url,
            "https://github.com/o/r/tree/main/skills/pdf"
        );
    }

    #[test]
    fn absent_agents_are_named_not_counted_away() {
        assert_eq!(absent_sentence(&[]), "");
        assert_eq!(
            absent_sentence(&["Cursor"]),
            "Cursor is not installed on this machine, so it has no row here."
        );
        assert_eq!(
            absent_sentence(&["Cursor", "Gemini CLI", "Amp"]),
            "These agents are not installed on this machine, so they have no rows here: Cursor, \
             Gemini CLI, Amp."
        );
    }

    /// The `SKILL.md` tab is the only one the pane's own editor writes, so it
    /// is the only one that can wear the dot. The Overview saves the same file
    /// but never edits it, and a dot there would point at a tab with nothing
    /// in it to fix.
    #[test]
    fn only_the_skill_file_tab_wears_the_dot() {
        let overview = Showing::Overview;
        let file = Showing::File(SKILL_FILE_NAME.into());

        // Nothing typed anywhere.
        assert!(!overview.dot(false));
        assert!(!file.dot(false));

        // Typed into the editor. The Overview reads the same file's
        // frontmatter, but the user did not type there.
        assert!(!overview.dot(true));
        assert!(file.dot(true));

        // A bundled file keeps its own dirty flag; this rule has nothing to say
        // about it.
        assert!(!Showing::File("references/api.md".into()).dot(true));
    }

    #[test]
    fn a_long_listing_arrives_closed_and_a_short_one_open() {
        assert!(files_open_by_default(0));
        assert!(files_open_by_default(1));
        assert!(files_open_by_default(MAX_ROWS_OPEN));
        // `docx` is thirty rows. Open, it puts Location, Visible to and Source
        // a screen and a half down the Overview.
        assert!(!files_open_by_default(MAX_ROWS_OPEN + 1));
        assert!(!files_open_by_default(30));
    }

    #[test]
    fn the_files_heading_counts_files_apart_from_folders() {
        fn node(rel: &str, is_dir: bool) -> FileNode {
            FileNode {
                rel: rel.into(),
                label: rel.into(),
                depth: 0,
                is_dir,
                editable: !is_dir,
            }
        }

        // A directory that could not be read says so under the heading, so the
        // heading itself has nothing to add.
        assert_eq!(files_summary(&[]), None);
        assert_eq!(
            files_summary(&[node(SKILL_FILE_NAME, false)]),
            Some("1 file".to_string())
        );
        assert_eq!(
            files_summary(&[node(SKILL_FILE_NAME, false), node("notes.md", false)]),
            Some("2 files".to_string())
        );
        // The folders are rows of the listing but not things to open, so they
        // are counted apart from the files rather than added to them.
        assert_eq!(
            files_summary(&[
                node(SKILL_FILE_NAME, false),
                node("references", true),
                node("references/api.md", false),
            ]),
            Some("2 files in 1 folder".to_string())
        );
        assert_eq!(
            files_summary(&[
                node(SKILL_FILE_NAME, false),
                node("references", true),
                node("scripts", true),
            ]),
            Some("1 file in 2 folders".to_string())
        );
    }

    #[test]
    fn a_list_of_phrases_reads_as_a_sentence() {
        let one = ["1 link".to_string()];
        let two = ["1 link".to_string(), "2 duplicate directories".to_string()];
        let three = [
            "the directory it lives in".to_string(),
            "3 links".to_string(),
            "1 duplicate directory".to_string(),
        ];

        assert_eq!(in_a_list(&[]), "");
        assert_eq!(in_a_list(&one), "1 link");
        assert_eq!(in_a_list(&two), "1 link and 2 duplicate directories");
        assert_eq!(
            in_a_list(&three),
            "the directory it lives in, 3 links, and 1 duplicate directory"
        );
    }
}
