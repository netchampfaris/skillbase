//! The detail pane: the selected skill's name and description as form fields,
//! the agents it is visible to, the whole `SKILL.md` in a code editor, and the
//! actions that change any of it on disk.
//!
//! Every write goes through `skillbase-core` against the one [`Roots`] the
//! application resolved at startup, and every write happens on a background
//! task. When the disk changes, this pane emits [`DetailEvent::Changed`] and
//! the root view scans again, so the interface never asserts a state the
//! filesystem does not back up.

use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{
    Editor, EditorState, Input, InputEvent, InputState, Textarea, TextareaState,
};
use gpui_kit::component::label::Label;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, ElementId, Entity, EventEmitter, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, px, rems,
};
use skillbase_core::{
    AgentDef, ConsolidatePlan, DeletePlan, DisableMode, Duplicate, InstallError, Installer,
    LocationKind, Outcome, Registry, Roots, SKILL_FILE_NAME, STORE_ID, Skill, SkillDoc, SkillError,
};

use super::model::{Issue, Scan, SkillView, agent_label, display_path, has_disable_state};
use super::report;
use super::{BAND_HEIGHT, agent_icon, drag_band};

/// How many differing paths a duplicate lists before it starts counting.
const DIFF_PATHS: usize = 6;

/// What this pane tells the root view.
pub enum DetailEvent {
    /// The filesystem changed. Scan again, and land on `select` afterwards.
    Changed { select: Option<SharedString> },
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

/// The right-hand pane.
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

/// A bundled file open in a tab, with its own editor.
///
/// `SKILL.md` is not one of these. Its frontmatter is what the Overview's Name
/// and Description fields edit, so it keeps the dedicated `body` editor and the
/// existing save path. Everything else is a plain text file: read, edited, and
/// written back with nothing in between.
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
    /// The frontmatter values as they were when the file was read, so that a
    /// form field can be told apart from one the user has not touched.
    loaded_name: SharedString,
    loaded_description: SharedString,
    /// Everything in the skill directory, flattened depth-first. The rows the
    /// user clicks to open a file.
    tree: Vec<FileNode>,
    /// Which tab is showing.
    showing: Showing,
    /// The bundled files the user has opened, one editor each. `SKILL.md` is
    /// not among them: it has its own editor and its own save path.
    open: Vec<OpenFile>,
    /// The skill's duplicate directories and how each compares to the origin.
    duplicates: Duplicates,
    /// Whether the "Visible to" section is open. Closed on every selection,
    /// because the pane's usual job is the editor below it.
    visibility_open: bool,
    /// Bumped on every comparison, so one that lands after the selection moved
    /// on is dropped.
    duplicates_generation: u64,
    name: Entity<InputState>,
    description: Entity<TextareaState>,
    body: Entity<EditorState>,
    dirty: bool,
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
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("kebab-case-name"));
        let description = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("What the skill does, and when an agent should load it.")
        });
        let body = cx.new(|cx| EditorState::new(window, cx).language("markdown"));

        let subscriptions = vec![
            cx.subscribe(&name, |this, _, event: &InputEvent, cx| {
                this.mark_edited(event, cx)
            }),
            cx.subscribe(&description, |this, _, event: &InputEvent, cx| {
                this.mark_edited(event, cx)
            }),
            cx.subscribe(&body, |this, _, event: &InputEvent, cx| {
                this.mark_edited(event, cx)
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
            open: Vec::new(),
            duplicates: Duplicates::None,
            duplicates_generation: 0,
            visibility_open: false,
            name,
            description,
            body,
            dirty: false,
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
            // The open files belong to the skill that was showing. Another
            // skill's directory has its own, so the tabs close with it and the
            // pane comes back to the Overview.
            self.open.clear();
            self.showing = Showing::Overview;
        }

        // The duplicate comparison is against the disk, not against the
        // editor, so it is refreshed on every show — including the one that
        // follows a mutation, which is exactly when it has changed.
        self.compare_duplicates(window, cx);

        // Re-reading the file under an unsaved edit would throw the edit away.
        // A mutation that only moved links leaves the bytes alone, so keep what
        // the editor holds and just refresh the metadata around it.
        if same_file && (self.dirty || matches!(self.source, Source::Loaded)) {
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
        self.dirty = false;

        let Some(skill) = self.skill.clone() else {
            self.source = Source::Empty;
            self.tree.clear();
            self.set_fields("", "", "", window, cx);
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
                        this.tree = loaded.1;
                        this.adopt_source(&text, window, cx);
                    }
                    Err(error) => {
                        this.tree.clear();
                        this.source = Source::Failed(error.to_string().into());
                        this.set_fields("", "", "", window, cx);
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
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

    /// Put the file into the editor and the two form fields.
    fn adopt_source(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let parsed = SkillDoc::parse(text).ok();
        let name = parsed
            .as_ref()
            .and_then(|doc| doc.frontmatter.name())
            .unwrap_or_default()
            .to_string();
        let description = parsed
            .as_ref()
            .and_then(|doc| doc.frontmatter.description())
            .unwrap_or_default()
            .to_string();

        self.loaded_name = name.clone().into();
        self.loaded_description = description.clone().into();
        self.source = Source::Loaded;
        self.dirty = false;
        self.set_fields(&name, &description, text, window, cx);
        cx.notify();
    }

    fn set_fields(
        &mut self,
        name: &str,
        description: &str,
        body: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // `set_value` does not emit a change event, so loading never marks the
        // pane dirty.
        self.name.update(cx, |state, cx| {
            state.set_value(name.to_string(), window, cx)
        });
        self.description.update(cx, |state, cx| {
            state.set_value(description.to_string(), window, cx)
        });
        self.body.update(cx, |state, cx| {
            state.set_value(body.to_string(), window, cx)
        });
    }

    fn mark_edited(&mut self, event: &InputEvent, cx: &mut Context<Self>) {
        if matches!(event, InputEvent::Change) && !self.dirty {
            self.dirty = true;
            cx.notify();
        }
    }

    // ---------------------------------------------------------------- saving

    /// Write the edits back through `skillbase-core`.
    ///
    /// **The editor holds the document.** Its text is parsed and becomes the
    /// file. A form field is applied on top of it only when the user actually
    /// changed that field — comparing it against the value the file had when it
    /// was loaded. So editing `description:` in the editor is honoured, editing
    /// the Description field is honoured, and when both were edited the form
    /// field wins, because it is the control built for that key.
    ///
    /// A file that does not parse cannot be saved; the parse error comes back
    /// as a notification and nothing is written.
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(skill), false) = (self.skill.clone(), self.busy) else {
            return;
        };

        let text = self.body.read(cx).value().to_string();
        let name_field = self.name.read(cx).value().trim().to_string();
        let description_field = self.description.read(cx).value().to_string();
        let name_edited = name_field != self.loaded_name.as_ref();
        let description_edited = description_field != self.loaded_description.as_ref();
        let dir = skill.origin.clone();
        let roots = self.roots.clone();

        self.busy = true;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let written = cx
                .background_spawn(async move {
                    let mut doc = SkillDoc::parse(&text)?;
                    if name_edited {
                        doc.frontmatter.set_name(&name_field);
                    }
                    if description_edited {
                        doc.frontmatter.set_description(&description_field);
                    }
                    Skill::new(&dir, doc).save()?;
                    // Read it back: what the pane shows next is what the disk
                    // holds, not what was sent to it.
                    let file = dir.join(SKILL_FILE_NAME);
                    let after = fs::read_to_string(&file).map_err(|e| SkillError::io(&file, e))?;
                    Ok::<_, SkillError>((file, after))
                })
                .await;

            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match written {
                    Ok((file, after)) => {
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
                        cx.emit(DetailEvent::Changed { select: name });
                    }
                    Err(error) => {
                        window.push_notification(
                            Notification::error(error.to_string()).title("Could not save"),
                            cx,
                        );
                        cx.notify();
                    }
                }
            })
            .ok();
            drop(roots);
        })
        .detach();
    }

    // ------------------------------------------------------------- mutations

    /// Run one filesystem operation on a background thread and report it.
    fn run<F>(&mut self, title: &'static str, op: F, window: &mut Window, cx: &mut Context<Self>)
    where
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
                report(title, result, &this.roots, window, cx);
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
        let title = if on { "Linked" } else { "Unlinked" };

        self.run(
            title,
            move |installer| {
                if on {
                    installer.link(&name, &origin, agent)
                } else {
                    let mut outcome = Outcome::default();
                    if parked {
                        outcome
                            .changes
                            .extend(installer.enable(&name, &origin, agent)?.changes);
                    }
                    outcome
                        .changes
                        .extend(installer.unlink(&name, agent)?.changes);
                    Ok(outcome)
                }
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

        self.run(
            if on { "Enabled" } else { "Disabled" },
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

    fn adopt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(skill) = self.skill.clone() else {
            return;
        };
        let name = skill.name.to_string();
        let origin = skill.origin.clone();
        self.run(
            "Adopted",
            move |installer| installer.adopt(&name, &origin),
            window,
            cx,
        );
    }

    fn release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(skill) = self.skill.clone() else {
            return;
        };
        let name = skill.name.to_string();
        // Release puts the origin back where adoption found it: the symlink it
        // left behind. Any link outside the store will do; the first one is the
        // one adoption wrote.
        let Some(dest) = skill
            .locations
            .iter()
            .find(|l| !l.path.starts_with(self.roots.store_dir()))
            .map(|l| l.path.clone())
        else {
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
            move |installer| installer.release(&name, &dest),
            window,
            cx,
        );
    }

    /// Count what a delete would remove, then confirm with those numbers.
    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let origin_line = match &plan.origin {
            Some(path) => format!("the directory {}", display_path(path, &roots)),
            None => "nothing that Skillbase owns".to_string(),
        };
        let summary = format!(
            "Deleting {name} removes {origin_line}, {} link{}, and {} duplicate director{}.{}",
            plan.link_count(),
            if plan.link_count() == 1 { "" } else { "s" },
            plan.copy_count(),
            if plan.copy_count() == 1 { "y" } else { "ies" },
            if plan.skipped.is_empty() {
                String::new()
            } else {
                format!(
                    " {} path{} outside every directory Skillbase manages will be left alone.",
                    plan.skipped.len(),
                    if plan.skipped.len() == 1 { "" } else { "s" }
                )
            }
        );

        let this = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let plan = plan.clone();
            let this = this.clone();
            alert
                .title("Delete this skill?")
                .description(summary.clone())
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Delete")
                        .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
                        .cancel_text("Keep")
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    this.update(cx, |this, cx| {
                        let plan = plan.clone();
                        this.run(
                            "Deleted",
                            move |installer| installer.delete(&plan),
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
                .title(format!("Consolidate {name}?"))
                .description(description)
                .width(px(520.))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(format!("Replace {replace_count}"))
                        .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
                        .cancel_text("Cancel")
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    this.update(cx, |this, cx| {
                        let plan = plan.clone();
                        this.run(
                            "Consolidated",
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

    fn reveal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(skill) = self.skill.clone() else {
            return;
        };
        let dir = skill.origin.clone();
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

    // ----------------------------------------------------------- presentation

    fn empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .bg(cx.theme().background)
            .child(
                Icon::new(IconName::FileText)
                    .large()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(div().text_sm().child("No skill selected"))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Pick a skill from the list to read or edit it."),
            )
            .into_any_element()
    }

    fn header(
        &self,
        skill: &SkillView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let invalid = skill.parse_error.is_some();

        drag_band("detail-header", window, cx)
            .flex_shrink_0()
            .h(BAND_HEIGHT)
            .px_5()
            .gap_3()
            .items_center()
            .justify_between()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .min_w_0()
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
                    .child(Tag::secondary().small().child(if skill.managed {
                        "Managed"
                    } else {
                        "Unmanaged"
                    })),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("reveal")
                            .ghost()
                            .small()
                            .icon(IconName::FolderOpen)
                            .tooltip("Reveal the origin folder")
                            .on_click(cx.listener(|this, _, window, cx| this.reveal(window, cx))),
                    )
                    .child(
                        Button::new("delete")
                            .ghost()
                            .small()
                            .icon(IconName::Delete)
                            .tooltip("Delete this skill")
                            .disabled(self.busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.confirm_delete(window, cx)),
                            ),
                    )
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

    /// The "Visible to" section, closed until asked for.
    ///
    /// It used to sit open between the description and the editor, a column of
    /// a dozen switches and a paragraph of explanation under each. Editing the
    /// file is what this pane is mostly for, and that column pushed the editor
    /// off the bottom of the window. Closed, the header still answers the
    /// question the section exists to answer — who can see this — and opening
    /// it is one click.
    fn visibility(&self, skill: &SkillView, cx: &mut Context<Self>) -> impl IntoElement {
        let managed = skill.managed;
        let open = self.visibility_open;
        let shared = Registry::shared();

        // Agents that exist on this machine, plus any that already hold a link
        // to this skill. Zed is never here: its directory is the shared
        // directory, so linking "into Zed" is the Shared switch.
        let installed = self
            .scan
            .as_ref()
            .map(|scan| scan.installed.clone())
            .unwrap_or_default();
        let agents: Vec<&'static AgentDef> = Registry::link_targets()
            .filter(|agent| !agent.is_shared())
            .filter(|agent| installed.contains(agent) || skill.linked_to(agent.id))
            .collect();

        v_flex()
            .flex_shrink_0()
            .gap_3()
            .child(
                h_flex()
                    .id("visibility-header")
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(
                        Icon::new(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .xsmall()
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
                            .child(reach(skill, cx)),
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
                                    .child(
                                        "This skill's directory is not in Skillbase's store, so \
                                         another tool may own it. Visibility is read-only until \
                                         you adopt it.",
                                    ),
                            )
                        })
                        .child(
                            v_flex()
                                .gap_1()
                                .child(
                                    Switch::new("visible-shared")
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
                                .child(
                                    div()
                                        .id("shared-effect")
                                        .pl_10()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        // Fourteen agent names is two lines of
                                        // text for a fact the reader can take
                                        // on trust; the names are a hover away.
                                        .tooltip({
                                            let names = covered.join(", ");
                                            move |window, cx| {
                                                Tooltip::new(names.clone()).build(window, cx)
                                            }
                                        })
                                        .child(format!(
                                            "Links {shared_path}, which {} agents read.",
                                            covered.len()
                                        )),
                                ),
                        )
                        .child(
                            v_flex().gap_2().children(
                                agents
                                    .into_iter()
                                    .map(|agent| self.agent_row(skill, agent, cx)),
                            ),
                        ),
                )
            })
    }

    /// One agent's row: its mark and name, what a switch there would do, and
    /// the one or two switches that do it.
    ///
    /// Presence and "switched on" are different questions for Claude Code and
    /// Codex, so they get two switches. They used to sit one above the other,
    /// the second indented under the first with its own paragraph of
    /// explanation — which read as a hierarchy that is not really there, and
    /// made two agents' rows four times the height of everybody else's. Both
    /// switches now sit on one line in fixed lanes, and the explanation is a
    /// tooltip on the switch it explains.
    fn agent_row(
        &self,
        skill: &SkillView,
        agent: &'static AgentDef,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let managed = skill.managed;
        let present = skill.linked_to(agent.id);
        let via_shared = skill.via_shared(agent);
        let dir = display_path(
            &self.roots.agent_dir(agent).join(skill.name.as_ref()),
            &self.roots,
        );

        let effect = match skill.location_kind(agent.id) {
            Some(LocationKind::Origin) => format!("The origin directory itself, at {dir}"),
            Some(LocationKind::Symlink { .. }) => format!("Linked at {dir}"),
            Some(LocationKind::Copy) => format!("A separate copy at {dir}, not a link"),
            Some(LocationKind::Disabled) => format!("Parked out of the way; {dir} is empty"),
            None if via_shared => {
                format!("Reached through Shared. A switch here would also link {dir}.")
            }
            None => format!("Links {dir}"),
        };

        let switchable = has_disable_state(agent) && (present || via_shared);
        // Codex's off state is a line in its own config file, so writing it
        // does not disturb whoever owns the skill. Claude Code's is a move
        // between directories, which is a visibility change, and §3.2 keeps
        // those read-only for a skill Skillbase does not own.
        let locked = !managed && matches!(agent.disable, DisableMode::MoveAside(_));
        let enabled = skill.enabled_for(agent);
        let how = disable_effect(agent, &self.roots);

        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .items_center()
                            .child(agent_icon(agent).xsmall())
                            .child(div().min_w_0().truncate().child(agent.display_name)),
                    )
                    // Two fixed lanes, so the presence switch lands on the same
                    // column in every row whether or not the agent has an
                    // "enabled" state, and both switches carry their own label
                    // rather than sharing one between them.
                    .child(h_flex().flex_shrink_0().w(rems(6.5)).justify_end().when(
                        switchable,
                        |this| {
                            this.child(
                                Switch::new((ElementId::from("enabled"), agent.id))
                                    .checked(enabled)
                                    .disabled(locked || self.busy)
                                    .label("Enabled")
                                    .tooltip(how)
                                    .on_click(cx.listener(
                                        move |this, checked: &bool, window, cx| {
                                            this.set_enabled(agent, *checked, window, cx)
                                        },
                                    )),
                            )
                        },
                    ))
                    .child(
                        h_flex().flex_shrink_0().w(rems(5.5)).justify_end().child(
                            Switch::new((ElementId::from("visible"), agent.id))
                                .checked(present)
                                .disabled(!managed || self.busy)
                                .label("Visible")
                                .on_click(cx.listener(move |this, checked: &bool, window, cx| {
                                    this.set_present(agent, *checked, window, cx)
                                })),
                        ),
                    ),
            )
            .child(
                div()
                    .pl_6()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(effect),
            )
            .into_any_element()
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
                    .p_3()
                    .gap_3()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().group_box)
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
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(if skill.managed {
                                "Skillbase owns this directory and may add or remove links to it."
                            } else {
                                "Skillbase reads and edits this directory in place. Adopting it \
                                 moves it into ~/.skillbase/store and leaves a symlink behind."
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
                                 copies above.",
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
                                    .label("Reveal in Finder")
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.reveal(window, cx)),
                                    ),
                            )
                            .child(if skill.managed {
                                Button::new("release")
                                    .outline()
                                    .small()
                                    .label("Release")
                                    .tooltip("Move the directory back out of the store")
                                    .disabled(self.busy)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.release(window, cx)),
                                    )
                            } else {
                                Button::new("adopt")
                                    .primary()
                                    .small()
                                    .label("Adopt")
                                    .tooltip(
                                        "Move the directory into ~/.skillbase/store and leave a \
                                         symlink behind",
                                    )
                                    .disabled(self.busy)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.adopt(window, cx)),
                                    )
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
                                "Consolidate 1 copy".to_string()
                            } else {
                                format!("Consolidate {replace} copies")
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
                            .child(shown),
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
                    ),
            )
            .when(!identical, |this| {
                // Name the files, then offer the only way to overwrite them.
                // Not offering it would leave the user stuck; offering it
                // without naming what goes would be worse than not offering it.
                let paths: Vec<SharedString> = duplicate
                    .diff()
                    .paths()
                    .take(DIFF_PATHS)
                    .map(|path| SharedString::from(path.display().to_string()))
                    .collect();
                let more = duplicate.diff().total().saturating_sub(paths.len());

                this.child(
                    v_flex()
                        .pl_6()
                        .gap_1()
                        .child(
                            h_flex()
                                .flex_wrap()
                                .gap_1()
                                .children(
                                    paths
                                        .into_iter()
                                        .map(|path| Tag::secondary().xsmall().child(path)),
                                )
                                .when(more > 0, |this| {
                                    this.child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(format!("+{more}")),
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

    /// The skill directory, as rows the user can click to open a file.
    ///
    /// This replaces the row of tags that named the bundled files without
    /// letting the user do anything with them. A directory is a heading rather
    /// than a control: there is nothing to open, and a disclosure triangle over
    /// a listing this short would hide files behind a click for no gain.
    fn folder_structure(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_shrink_0()
            .gap_2()
            .child(section_title("Files", cx))
            .child(if self.tree.is_empty() {
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("The skill directory could not be read.")
                    .into_any_element()
            } else {
                v_flex()
                    // A skill with thirty reference files would otherwise push
                    // everything below it off the tab. Past about a dozen rows
                    // the list scrolls in place instead.
                    .id("file-tree")
                    .max_h(rems(19.))
                    .overflow_y_scroll()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().group_box)
                    .children(self.tree.clone().iter().map(|node| self.file_row(node, cx)))
                    .into_any_element()
            })
    }

    fn file_row(&self, node: &FileNode, cx: &mut Context<Self>) -> AnyElement {
        let showing = self.is_open(&node.rel);
        let rel = node.rel.clone();
        // One step on the spacing scale per level: enough to read as nesting
        // without pushing a deep file off the pane.
        let indent = rems(0.5 + 0.75 * node.depth as f32);

        let row = h_flex()
            .id(ElementId::from((
                ElementId::from("file-row"),
                node.rel.clone(),
            )))
            .w_full()
            .h_7()
            .pr_2()
            .pl(indent)
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
                    .text_sm()
                    .when(node.is_dir, |this| {
                        this.text_color(cx.theme().muted_foreground)
                    })
                    .child(node.label.clone()),
            );

        if node.is_dir {
            return row.into_any_element();
        }

        if !node.editable {
            // Nothing an editor can do with it, and saying so beats a row that
            // looks clickable and then refuses.
            return row
                .child(
                    div()
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("not text"),
                )
                .into_any_element();
        }

        row.when(showing, |this| {
            this.child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("open"),
            )
        })
        .hover(|this| this.bg(cx.theme().list_hover))
        .on_click(cx.listener(move |this, _, window, cx| this.open_file(rel.clone(), window, cx)))
        .into_any_element()
    }

    // ------------------------------------------------------------------ tabs

    /// True when `rel` has a tab, whether or not it is the one showing.
    fn is_open(&self, rel: &SharedString) -> bool {
        rel == SKILL_FILE_NAME || self.open.iter().any(|file| &file.rel == rel)
    }

    /// Whether the tab now showing has edits that have not been written.
    fn showing_dirty(&self) -> bool {
        match &self.showing {
            // The Overview edits `SKILL.md`'s frontmatter, so it saves what the
            // `SKILL.md` tab saves and is dirty when it is.
            Showing::Overview => self.dirty,
            Showing::File(rel) if rel == SKILL_FILE_NAME => self.dirty,
            Showing::File(rel) => self
                .open
                .iter()
                .find(|file| &file.rel == rel)
                .is_some_and(|file| file.dirty),
        }
    }

    /// Save whatever the active tab holds.
    ///
    /// The Overview and the `SKILL.md` tab both save through [`Self::save`]:
    /// they are two views of one file, the Overview editing its frontmatter and
    /// the tab its text, and that one write reconciles them.
    fn save_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.showing.clone() {
            Showing::Overview => self.save(window, cx),
            Showing::File(rel) if rel == SKILL_FILE_NAME => self.save(window, cx),
            Showing::File(rel) => self.save_file(rel, window, cx),
        }
    }

    /// Open a bundled file in a tab, or show the tab it already has.
    ///
    /// `SKILL.md` is never read again here: it has a tab from the moment the
    /// skill is selected, and re-reading it would throw away an unsaved edit.
    fn open_file(&mut self, rel: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        if rel == SKILL_FILE_NAME || self.open.iter().any(|file| file.rel == rel) {
            self.showing = Showing::File(rel);
            cx.notify();
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
        self.showing = Showing::File(rel.clone());
        cx.notify();

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

    /// Close a bundled file's tab, discarding whatever it held.
    ///
    /// An unsaved edit is confirmed first, because a tab's close button is a
    /// small target next to the label and hitting it by accident should not
    /// cost the user their work.
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

        let this = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            let rel = rel.clone();
            alert
                .title(format!("Close {rel} without saving?"))
                .description("The edits in this tab have not been written to disk.")
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Discard")
                        .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
                        .cancel_text("Keep editing")
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    this.update(cx, |this, cx| this.drop_file(&rel, cx)).ok();
                    true
                })
        });
    }

    fn drop_file(&mut self, rel: &SharedString, cx: &mut Context<Self>) {
        self.open.retain(|file| &file.rel != rel);
        if self.showing == Showing::File(rel.clone()) {
            // Back to the Overview rather than to a neighbouring tab: which
            // neighbour is arbitrary, and the Overview is where the file list
            // is, so it is where the user goes next either way.
            self.showing = Showing::Overview;
        }
        cx.notify();
    }

    /// Write the bundled file showing in the active tab back to disk.
    ///
    /// Nothing parses it and nothing rewrites it. A bundled file is whatever
    /// the skill's author put there, so what the editor holds is what is
    /// written.
    fn save_file(&mut self, rel: SharedString, window: &mut Window, cx: &mut Context<Self>) {
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
    fn tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut labels: Vec<(Showing, SharedString, bool, bool)> = vec![
            (Showing::Overview, "Overview".into(), self.dirty, false),
            (
                Showing::File(SKILL_FILE_NAME.into()),
                SKILL_FILE_NAME.into(),
                self.dirty,
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

        let selected = labels
            .iter()
            .position(|(showing, ..)| showing == &self.showing)
            .unwrap_or(0);
        let order: Vec<Showing> = labels.iter().map(|(showing, ..)| showing.clone()).collect();
        let this = cx.entity().downgrade();

        TabBar::new("detail-tabs")
            // Segmented rather than the boxed default: the boxed variant marks
            // the selected tab by weight alone against this theme, and the
            // segmented one's trough gives this second band a shape of its own
            // under the header without a rule between the two.
            .segmented()
            // Enough open files to overflow the pane get a dropdown rather
            // than being clipped off the right edge. The trigger is drawn
            // unconditionally, though, and inside the segmented trough it
            // reads as a select rather than as an overflow, so it is asked for
            // only once there are enough tabs to be worth one.
            .menu(labels.len() > 4)
            .selected_index(selected)
            .children(labels.into_iter().map(|(showing, label, dirty, closable)| {
                Tab::new().label(label).suffix(
                    h_flex()
                        .gap_1()
                        .items_center()
                        // A fixed lane, so a tab does not resize when its dot
                        // appears and the strip does not shuffle under the
                        // pointer as the user types.
                        .when(dirty, |this| {
                            this.child(
                                div()
                                    .size(px(6.))
                                    .rounded_full()
                                    .bg(cx.theme().muted_foreground),
                            )
                        })
                        .when(closable, |this| {
                            let Showing::File(rel) = showing.clone() else {
                                return this;
                            };
                            this.child(
                                div()
                                    .id(ElementId::from((
                                        ElementId::from("close-tab"),
                                        rel.clone(),
                                    )))
                                    .flex_shrink_0()
                                    .rounded(cx.theme().radius)
                                    .hover(|this| this.bg(cx.theme().list_hover))
                                    .child(
                                        Icon::new(IconName::Close)
                                            .xsmall()
                                            .text_color(cx.theme().muted_foreground),
                                    )
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        // Without this the tab strip would also
                                        // read the click and select the tab
                                        // that is on its way out.
                                        cx.stop_propagation();
                                        this.close_file(rel.clone(), window, cx);
                                    })),
                            )
                        }),
                )
            }))
            .on_click(move |index, _, cx| {
                let Some(showing) = order.get(*index).cloned() else {
                    return;
                };
                this.update(cx, |this, cx| {
                    this.showing = showing;
                    cx.notify();
                })
                .ok();
            })
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

/// What turning an agent's Enabled switch off actually does.
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
        STORE_ID => "Skillbase store",
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
fn reach(skill: &SkillView, cx: &mut Context<DetailPane>) -> SharedString {
    const NAMED: usize = 3;

    let mut names: Vec<&'static str> = Vec::new();
    if skill.in_shared {
        names.push("Shared");
    }
    names.extend(skill.visible_to.iter().copied().map(agent_label));

    let _ = cx;
    match names.len() {
        0 => "No agent can see this".into(),
        n if n <= NAMED => names.join(", ").into(),
        n => format!("{}, +{}", names[..NAMED].join(", "), n - NAMED).into(),
    }
}

fn section_title(label: &'static str, cx: &mut Context<DetailPane>) -> impl IntoElement {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(label)
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
        _ => return None,
    })
}

/// Open a directory in the platform's file manager.
fn open_in_file_manager(dir: &Path) -> Result<(), String> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program)
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
            return self.empty_state(cx).into_any_element();
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
            .child(
                h_flex()
                    .flex_shrink_0()
                    .h_11()
                    .px_5()
                    .items_center()
                    .child(self.tabs(cx)),
            )
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }
}

impl DetailPane {
    /// The first tab: what the skill is, what is in its directory, and who can
    /// see it.
    ///
    /// The file itself is not here. It used to be, under a heading at the
    /// bottom of a column that had to be scrolled past the description, the
    /// switches and the duplicate list to reach. It now has a tab of its own.
    fn overview(&self, skill: &SkillView, cx: &mut Context<Self>) -> AnyElement {
        let loading = matches!(self.source, Source::Loading);
        let read_error = match &self.source {
            Source::Failed(error) => Some(error.clone()),
            _ => None,
        };

        v_flex()
            // One scroll owner for the tab.
            .id("detail-overview")
            .size_full()
            .overflow_y_scroll()
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
                                    "The Name and Description fields stay empty until the \
                                     frontmatter parses. Fix it in the SKILL.md tab and save.",
                                ),
                        ),
                )
            })
            .child(
                v_flex()
                    .flex_shrink_0()
                    .gap_4()
                    .child(
                        v_flex()
                            .gap_2()
                            .child(Label::new("Name"))
                            .child(Input::new(&self.name).small().disabled(loading))
                            .children(issue_lines(issues_for(&skill.issues, "name"), cx)),
                    )
                    .child(
                        v_flex()
                            .gap_2()
                            .child(Label::new("Description"))
                            .child(Textarea::new(&self.description).h(rems(4.5)))
                            .children(issue_lines(issues_for(&skill.issues, "description"), cx)),
                    ),
            )
            .child(self.folder_structure(cx))
            .child(self.visibility(skill, cx))
            .child(self.duplicates_section(cx))
            .child(self.location(skill, cx))
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
