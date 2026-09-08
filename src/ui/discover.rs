//! The Discover view: search skills.sh, and install what it finds.
//!
//! It replaces the list and the detail pane rather than living inside them.
//! The three-pane layout is about skills that are already on this machine —
//! the list filters them, the detail pane edits one — and nothing here is on
//! the machine yet. A row is a candidate, not a skill.
//!
//! Two things reach the network from this module, and both are on background
//! tasks: the search, and the install that follows it. skills.sh answers the
//! first; every byte of a skill comes from GitHub, because the registry is a
//! index and not a mirror.
//!
//! An install is three steps, not one. First it settles what was asked for
//! against GitHub — the repository's real default branch when the user named
//! no ref, and the repository's own skills when the spelling names a whole
//! repository rather than one directory. Then it downloads, one skill at a
//! time. Then it says what stands. Every step is cancellable, and cancelling
//! writes nothing: the flag the task carries is read before the store is
//! touched.

use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::{DialogClose, DialogFooter};
use gpui_kit::component::input::Input;
use gpui_kit::component::label::Label;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, ElementId, InteractiveElement as _, IntoElement,
    ParentElement as _, PathPromptOptions, SharedString, Styled as _, Window, div, px, rems,
};
use skillbase_core::{
    DEFAULT_LIMIT, FetchError, GitHub, ImportOptions, InstallError, InstallOptions, Installed,
    Installer, MIN_QUERY_LEN, ParsedLocation, Roots, SearchHit, SearchResults, SkillLocation,
    SkillsSh, UreqHttp, resolve,
};

use crate::app::{Skillbase, WorkArea};

use super::model::display_path;
use super::{PAGE_MAX_WIDTH, capitalized, install_skill, report, report_install};

/// How long typing has to stop before a search is sent.
///
/// Long enough that a typed word is one request rather than one per letter,
/// short enough that the results feel like a response to the typing.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// How many failed skills a batch summary names before it counts the rest.
const NAMED_FAILURES: usize = 3;

/// Where the registry search has got to.
pub(crate) enum SearchState {
    /// Nothing has been asked for, or the query is too short to ask about.
    Idle,
    Searching,
    Ready(Rc<SearchResults>),
    Failed(SharedString),
}

/// Hands out a [`JobId`] to each job in turn.
static NEXT_JOB: AtomicU64 = AtomicU64::new(0);

/// Which job a slot holds.
///
/// A task that finishes late has to be able to tell its own job from the one
/// that took the slot after it, or it clears another job's progress and puts
/// that job's Cancel out of reach.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct JobId(u64);

/// A job in flight that writes into the store: a download, a batch update, or
/// a copy from a folder on this machine.
///
/// Held rather than a flag, because the interface has to name what it is
/// waiting for: without the name, a window that sits still for the two minutes
/// an HTTP request is allowed says nothing at all. One type for all three, so
/// the strip at the foot of the window cannot come to answer the same question
/// differently depending on which view started the work.
pub(crate) struct Installing {
    /// Tells this job apart from the next one to hold the same slot.
    id: JobId,
    /// What the job's sentence leads with, lower-case so it can sit inside a
    /// longer one: "downloading", "updating", "copying". The strip capitalizes
    /// it.
    verb: &'static str,
    /// The skill being worked on now, named as it will be named on disk.
    /// `None` until the job settles what that is.
    name: Option<SharedString>,
    /// What the strip says while there is nothing to name.
    waiting: SharedString,
    /// The Discover row this started from, so that one row stands down while
    /// the rest of the list stays live.
    row: Option<SharedString>,
    /// Which of `total` is being worked on now, counted from zero.
    index: usize,
    /// How many are already written, which is not the same thing: a skill that
    /// failed still moved the count of what has been tried.
    done: usize,
    /// How many were asked for.
    total: usize,
    /// Set to stop the work, or `None` for work that cannot be stopped
    /// part-way. The strip shows a Cancel only when there is a flag to set,
    /// because a Cancel that does nothing is worse than no Cancel.
    ///
    /// A download reads it between steps and writes nothing into the store
    /// once it is set, so dropping the task cannot leave half a skill in
    /// `~/.agents/skills`. A request already in flight still runs to its end —
    /// a blocking socket read cannot be interrupted — but its bytes are thrown
    /// away rather than installed.
    cancel: Option<Arc<AtomicBool>>,
    /// Whether the strip at the foot of the window shows this job.
    ///
    /// Holding the slot and being shown are separate: a copy of a handful of
    /// text files holds the slot for the moment it takes, and a strip that
    /// appears and vanishes in that moment is noise.
    shown: bool,
}

impl Installing {
    /// A job that has not named what it is working on yet. `verb` leads the
    /// strip's sentence once it has; `waiting` is what the strip says until
    /// then.
    pub(crate) fn new(verb: &'static str, waiting: impl Into<SharedString>) -> Self {
        Self {
            id: JobId(NEXT_JOB.fetch_add(1, Ordering::Relaxed)),
            verb,
            name: None,
            waiting: waiting.into(),
            row: None,
            index: 0,
            done: 0,
            total: 0,
            cancel: None,
            shown: true,
        }
    }

    /// A job the strip does not show until [`Installing::show`] is called.
    pub(crate) fn hidden(mut self) -> Self {
        self.shown = false;
        self
    }

    /// Which job this is, for a task that has to check the slot still holds
    /// the job it started.
    pub(crate) fn id(&self) -> JobId {
        self.id
    }

    /// Put this job in the strip.
    pub(crate) fn show(&mut self) {
        self.shown = true;
    }

    /// Whether the strip shows this job.
    pub(crate) fn shown(&self) -> bool {
        self.shown
    }

    /// How many the job was asked for, before the first one starts.
    pub(crate) fn of(mut self, total: usize) -> Self {
        self.total = total;
        self
    }

    /// The Discover row the job started from.
    pub(crate) fn from_row(mut self, row: Option<SharedString>) -> Self {
        self.row = row;
        self
    }

    /// The flag Cancel sets, for work that can be stopped between steps.
    pub(crate) fn cancelled_by(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Name what is being worked on now.
    pub(crate) fn working_on(&mut self, name: impl Into<SharedString>) {
        self.name = Some(name.into());
    }

    /// Where the job has got to: the `index` of `total` it is on, with `done`
    /// already written.
    pub(crate) fn reached(&mut self, index: usize, done: usize, total: usize) {
        self.index = index;
        self.done = done;
        self.total = total;
    }

    /// What the job is doing, phrased to sit inside a sentence:
    /// "downloading pdf".
    pub(crate) fn doing(&self) -> SharedString {
        match &self.name {
            Some(name) => format!("{} {name}", self.verb).into(),
            None => self.waiting.clone(),
        }
    }

    /// The same, as the strip's own sentence: "Downloading pdf".
    pub(crate) fn label(&self) -> SharedString {
        capitalized(&self.doing()).into()
    }

    /// The skill being worked on, for the sentences outside the strip that
    /// name it.
    pub(crate) fn name(&self) -> Option<&SharedString> {
        self.name.as_ref()
    }

    /// How many are already written.
    pub(crate) fn done(&self) -> usize {
        self.done
    }

    /// How many were asked for.
    pub(crate) fn total(&self) -> usize {
        self.total
    }

    /// True when this job is the one a Discover row started, so that row alone
    /// stands down.
    pub(crate) fn started_from(&self, row: &SharedString) -> bool {
        self.row.as_ref() == Some(row)
    }

    /// Ask the work to stop. Does nothing for work that cannot be stopped.
    pub(crate) fn stop(&self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Whether there is anything for a Cancel button to set.
    pub(crate) fn stoppable(&self) -> bool {
        self.cancel.is_some()
    }

    /// Which of how many, when there is more than one to count.
    fn count(&self) -> Option<SharedString> {
        (self.total > 1).then(|| format!("{} of {}", self.index + 1, self.total).into())
    }
}

/// One skill a repository offers, and whether it is ticked for install.
pub(crate) struct InstallChoice {
    location: SkillLocation,
    selected: bool,
}

/// What an install task was asked to do.
enum InstallPlan {
    /// Ask GitHub where this actually is before downloading anything.
    Settle(Settle),
    /// Settled already: install these, in this order.
    Ready(Vec<SkillLocation>),
}

/// A request that still has to be settled against GitHub.
enum Settle {
    /// A search result. It names a repository and a skill id and no path, so
    /// the repository has to be looked at.
    Hit(SearchHit),
    /// A typed spec. Its ref may be a guess, and it may name a whole
    /// repository rather than one skill.
    Spec(ParsedLocation),
}

/// What settling worked out.
enum Settled {
    /// Install exactly this.
    One(SkillLocation),
    /// Put these to the user: more than one skill answers what was asked for.
    Choose {
        title: SharedString,
        lead: SharedString,
        locations: Vec<SkillLocation>,
    },
    /// There is nothing to install, and this says why.
    Nothing(SharedString),
}

/// A skill whose name in the store is already taken.
struct Occupied {
    /// Where the skill that could not be written was coming from.
    source: OccupiedSource,
    /// The name that is taken.
    name: SharedString,
    /// The directory in the way.
    path: PathBuf,
    /// The first free `name-2`, `name-3`, … to offer as a second copy.
    free_name: String,
}

/// The two ways a skill arrives, and the two ways the same refusal is answered.
///
/// The question the dialog puts is identical — replace what is there, keep
/// both, or do nothing — so the dialog is one. Only the retry differs.
enum OccupiedSource {
    /// A download from GitHub.
    Download(SkillLocation),
    /// A folder the user picked on this machine.
    Folder(PathBuf),
}

impl Skillbase {
    pub(crate) fn render_discover(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // Discover takes the whole work area, so with the sidebar hidden this
        // band is the leftmost one and has to leave the traffic lights room.
        let title_row = h_flex()
            .h_full()
            .w_full()
            .px_5()
            .gap_2()
            .items_center()
            .children(self.sidebar_reopen(cx))
            .child(div().text_base().font_medium().child("Discover"));

        v_flex()
            .size_full()
            .min_w_0()
            .bg(cx.theme().background)
            .child(self.column_band("discover-band", title_row, window, cx))
            .child(
                v_flex().flex_1().min_h_0().w_full().items_center().child(
                    v_flex()
                        .flex_1()
                        .min_h_0()
                        // Full width up to the cap rather than a fixed width:
                        // at the 860px minimum window the work area is
                        // narrower than PAGE_MAX_WIDTH, and a fixed w() would
                        // overflow it.
                        .w_full()
                        .max_w(px(PAGE_MAX_WIDTH))
                        .child(
                            h_flex().flex_shrink_0().h_11().px_5().items_center().child(
                                div().flex_1().min_w_0().child(
                                    Input::new(&self.discover_query)
                                        .small()
                                        .cleanable(true)
                                        .prefix(Icon::new(IconName::Search).small()),
                                ),
                            ),
                        )
                        .child(
                            div()
                                .id("discover-body")
                                .flex_1()
                                .min_h_0()
                                // With a bar: a full page of results runs
                                // well past the fold with nothing to say so.
                                .overflow_y_scrollbar()
                                .px_5()
                                .pb_8()
                                .child(self.discover_body(cx)),
                        ),
                ),
            )
    }

    /// The strip that says what is downloading, and offers a way to stop it.
    ///
    /// It sits under the columns rather than inside Discover, because an
    /// install can start from the menu bar with the skill list on screen. One
    /// place that always shows the answer beats one place per entry point.
    pub(crate) fn render_install_progress(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        // A job that holds the slot is not always a job worth a strip: a copy
        // from a folder holds it from the moment it is asked for, and says so
        // only once it turns out to be slow.
        let progress = self
            .installing
            .as_ref()
            .filter(|progress| progress.shown())?;
        Some(self.render_progress("install", progress, Self::cancel_install, cx))
    }

    /// The strip every job in flight shows: what it is doing, which of how
    /// many, and a way to stop it.
    ///
    /// One builder for all of them — a download, a batch update, a copy from a
    /// folder — because they answer the same question and only one of them can
    /// run at a time. `cancel` is what the button calls; a job with nothing to
    /// set shows no button.
    pub(crate) fn render_progress(
        &self,
        id: &'static str,
        progress: &Installing,
        cancel: fn(&mut Self, &mut Window, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .flex_shrink_0()
            .h_10()
            .w_full()
            .px_5()
            .gap_2()
            .items_center()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .child(Spinner::new().small())
            .child(div().text_sm().child(progress.label()))
            .children(progress.count().map(|count| {
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(count)
            }))
            .child(div().flex_1())
            .when(progress.stoppable(), |this| {
                this.child(
                    Button::new(SharedString::from(format!("cancel-{id}")))
                        .outline()
                        .small()
                        .label("Cancel")
                        .on_click(cx.listener(move |this, _, window, cx| cancel(this, window, cx))),
                )
            })
            .into_any_element()
    }

    fn discover_body(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.discover {
            SearchState::Idle => empty_state(
                "Search skills.sh",
                "Type at least two characters. The results come from the skills.sh registry; \
                 installing one downloads it from the repository the registry names."
                    .into(),
                None,
                cx,
            ),
            SearchState::Searching => v_flex()
                .py_2()
                .gap_4()
                .children((0..5).map(|row| {
                    v_flex()
                        .id(ElementId::from(("discover-skeleton", row as usize)))
                        .gap_2()
                        .child(Skeleton::new().h(rems(0.9)).w(rems(11.)))
                        .child(Skeleton::new().h(rems(0.8)).w(rems(16.)))
                }))
                .into_any_element(),
            // The query is still in the field, so asking again is one button
            // rather than retyping it: the search only fires on a change, and
            // a failure leaves nothing to change.
            SearchState::Failed(error) => empty_state(
                "skills.sh did not answer",
                error.clone(),
                Some(
                    Button::new("retry-search")
                        .outline()
                        .small()
                        .label("Try again")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.search_registry(window, cx)),
                        )
                        .into_any_element(),
                ),
                cx,
            ),
            SearchState::Ready(results) if results.is_empty() => empty_state(
                "Nothing matched",
                format!("skills.sh lists no skill for “{}”.", results.query).into(),
                None,
                cx,
            ),
            SearchState::Ready(results) => v_flex()
                .py_2()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "{} result{} from skills.sh. Installing one writes it into {}.",
                            results.skills.len(),
                            if results.skills.len() == 1 { "" } else { "s" },
                            display_path(&self.roots.store_dir(), &self.roots)
                        )),
                )
                .child(
                    v_flex()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().group_box)
                        .children(results.skills.iter().map(|hit| self.result_row(hit, cx))),
                )
                .into_any_element(),
        }
    }

    /// One search result.
    ///
    /// skills.sh returns no description, so there is nothing to put under the
    /// name. What the row can answer instead is which repository it comes from
    /// and how many people have installed it, which is the whole basis for
    /// choosing between two rows with similar names.
    fn result_row(&self, hit: &SearchHit, cx: &mut Context<Self>) -> AnyElement {
        let hit = hit.clone();
        let id = hit_id(&hit);
        // Only the row being downloaded stands down. A whole list greyed out
        // says nothing about which row is busy, and the rest of the list is
        // still worth reading while one skill downloads.
        let downloading = self
            .installing
            .as_ref()
            .is_some_and(|progress| progress.started_from(&id));

        h_flex()
            // Identity comes from the registry's own row id, so a row keeps its
            // state as results are replaced.
            .id(ElementId::from((
                ElementId::from("discover-row"),
                id.clone(),
            )))
            .w_full()
            .px_3()
            .py_2()
            .gap_3()
            .items_center()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .text_sm()
                            .font_medium()
                            .truncate()
                            .child(SharedString::from(hit.name.clone())),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(SharedString::from(hit.source.clone())),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(installs(hit.installs)),
            )
            .child(
                Button::new(ElementId::from((ElementId::from("discover-install"), id)))
                    .outline()
                    .small()
                    .label(if downloading {
                        "Downloading"
                    } else {
                        "Install"
                    })
                    .disabled(downloading)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.install_hit(hit.clone(), window, cx)
                    })),
            )
            .into_any_element()
    }

    /// Ask skills.sh what matches what has been typed, once the typing stops.
    ///
    /// Every keystroke calls this, and so does the Try again button. The
    /// generation counter is what makes that safe: it cancels the pending
    /// request before it is sent and drops a result that lands after a newer
    /// keystroke, so the rows always belong to the query in the field.
    pub(crate) fn search_registry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.discover_generation += 1;
        let generation = self.discover_generation;

        let query = self.discover_query.read(cx).value().trim().to_string();
        if query.chars().count() < MIN_QUERY_LEN {
            // The API refuses a shorter query, so the interface says what it
            // is waiting for rather than showing an error it caused itself.
            self.discover = SearchState::Idle;
            cx.notify();
            return;
        }

        self.discover = SearchState::Searching;
        cx.notify();

        self._discover_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let current = this
                .read_with(cx, |this, _| this.discover_generation == generation)
                .unwrap_or(false);
            if !current {
                return;
            }

            let results = cx
                .background_spawn(async move {
                    SkillsSh::new(UreqHttp::new()).search(&query, DEFAULT_LIMIT, None)
                })
                .await;

            this.update(cx, |this, cx| {
                if this.discover_generation != generation {
                    return;
                }
                this.discover = match results {
                    Ok(results) => SearchState::Ready(Rc::new(results)),
                    Err(error) => SearchState::Failed(error.to_string().into()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Work out where a search result actually lives, then install it.
    ///
    /// A hit names a repository and a directory name and nothing else, so the
    /// repository has to be looked at. It can hold more than one directory of
    /// that name, and only the user knows which was meant.
    fn install_hit(&mut self, hit: SearchHit, window: &mut Window, cx: &mut Context<Self>) {
        let name = SharedString::from(hit.name.clone());
        let row = hit_id(&hit);
        self.run_install(
            InstallPlan::Settle(Settle::Hit(hit)),
            InstallOptions::new(),
            name,
            Some(row),
            window,
            cx,
        );
    }

    /// Ask which of a repository's skills to install.
    ///
    /// Two directories named `pdf` in one repository is a real thing, and so is
    /// a repository whose forty skills all sit one directory down. Both land
    /// here, and both are put to the user rather than guessed at: each row
    /// names the path within the repository, which is the only thing that tells
    /// them apart, and more than one row can be ticked.
    fn open_location_dialog(
        &mut self,
        title: SharedString,
        lead: SharedString,
        locations: Vec<SkillLocation>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.install_choices = locations
            .into_iter()
            .map(|location| InstallChoice {
                location,
                selected: false,
            })
            .collect();
        cx.notify();

        let this = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let this = this.clone();
            let choices: Vec<(SharedString, bool)> = this
                .upgrade()
                .map(|entity| {
                    entity
                        .read(cx)
                        .install_choices
                        .iter()
                        .map(|choice| (choice_label(&choice.location), choice.selected))
                        .collect()
                })
                .unwrap_or_default();
            let ticked = choices.iter().filter(|(_, on)| *on).count();
            let all_ticked = !choices.is_empty() && ticked == choices.len();

            dialog
                .title(title.clone())
                .width(px(560.))
                .content({
                    let lead = lead.clone();
                    let choices = choices.clone();
                    let this = this.clone();
                    move |content, _, cx| {
                        let choices = choices.clone();
                        let this = this.clone();
                        content.child(
                            v_flex()
                                .p_4()
                                .gap_3()
                                .child(
                                    h_flex()
                                        .w_full()
                                        .gap_3()
                                        .items_start()
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .text_sm()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(lead.clone()),
                                        )
                                        .child(
                                            Button::new("toggle-all-choices")
                                                .ghost()
                                                .small()
                                                .label(if all_ticked {
                                                    "Clear"
                                                } else {
                                                    "Select all"
                                                })
                                                .on_click({
                                                    let this = this.clone();
                                                    move |_, _, cx| {
                                                        this.update(cx, |this, cx| {
                                                            for choice in &mut this.install_choices
                                                            {
                                                                choice.selected = !all_ticked;
                                                            }
                                                            cx.notify();
                                                        })
                                                        .ok();
                                                    }
                                                }),
                                        ),
                                )
                                .child(
                                    v_flex()
                                        .id("install-choices")
                                        .max_h(px(260.))
                                        .overflow_y_scrollbar()
                                        .rounded(cx.theme().radius)
                                        .bg(cx.theme().group_box)
                                        .children(choices.into_iter().map(|(path, on)| {
                                            let this = this.clone();
                                            let key = path.clone();
                                            h_flex()
                                                .id(ElementId::from((
                                                    ElementId::from("install-choice"),
                                                    path.clone(),
                                                )))
                                                .w_full()
                                                .px_3()
                                                .py_2()
                                                .child(
                                                    Checkbox::new(ElementId::from((
                                                        ElementId::from("install-choice-box"),
                                                        path.clone(),
                                                    )))
                                                    .checked(on)
                                                    .label(path)
                                                    .on_click(move |checked, _, cx| {
                                                        let checked = *checked;
                                                        let key = key.clone();
                                                        this.update(cx, |this, cx| {
                                                            for choice in &mut this.install_choices
                                                            {
                                                                if choice_label(&choice.location)
                                                                    == key
                                                                {
                                                                    choice.selected = checked;
                                                                }
                                                            }
                                                            cx.notify();
                                                        })
                                                        .ok();
                                                    }),
                                                )
                                        })),
                                ),
                        )
                    }
                })
                .footer(
                    DialogFooter::new()
                        .p_4()
                        .child(
                            DialogClose::new().child(
                                Button::new("cancel-install-choice")
                                    .outline()
                                    .label("Cancel"),
                            ),
                        )
                        .child(
                            Button::new("confirm-install-choice")
                                .primary()
                                .label(match ticked {
                                    0 => "Install".to_string(),
                                    1 => "Install 1 skill".to_string(),
                                    many => format!("Install {many} skills"),
                                })
                                .disabled(ticked == 0)
                                .on_click({
                                    let this = this.clone();
                                    move |_, window, cx| {
                                        this.update(cx, |this, cx| {
                                            let chosen: Vec<SkillLocation> = this
                                                .install_choices
                                                .iter()
                                                .filter(|choice| choice.selected)
                                                .map(|choice| choice.location.clone())
                                                .collect();
                                            this.install_choices.clear();
                                            if chosen.is_empty() {
                                                return;
                                            }
                                            let label = install_label(&chosen);
                                            this.run_install(
                                                InstallPlan::Ready(chosen),
                                                InstallOptions::new(),
                                                label,
                                                None,
                                                window,
                                                cx,
                                            );
                                        })
                                        .ok();
                                        window.close_dialog(cx);
                                    }
                                }),
                        ),
                )
        });
    }

    /// Say that the name is taken, and offer the three ways out.
    ///
    /// Refusing and printing the refusal is a dead end: the user asked for a
    /// skill they can see they already have, and the only thing they can do
    /// about it from a notification is nothing. Replace is recoverable —
    /// the directory it takes away is moved to the trash, not deleted — and
    /// Keep both writes the new copy beside the one already there.
    ///
    /// One dialog for a download and for a folder on this machine, because the
    /// question and the three answers are the same either way.
    fn open_occupied_dialog(
        &mut self,
        occupied: Occupied,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let where_from = self.provenance_sentence(&occupied);
        let occupied = Rc::new(occupied);
        let store = display_path(&occupied.path, &self.roots);
        let trash = display_path(&self.roots.trash_dir(), &self.roots);
        let this = cx.entity().downgrade();

        window.open_dialog(cx, move |dialog, _, _| {
            let occupied = occupied.clone();
            let this = this.clone();
            let title = format!("“{}” is already installed", occupied.name);
            let lead = format!("A skill called {} is already in {store}.", occupied.name);
            let where_from = where_from.clone();
            let consequence = format!(
                "Replace moves the copy that is there into {trash}, where it can be got back. \
                 Keep both writes {} as {}.",
                match occupied.source {
                    OccupiedSource::Download(_) => "the download",
                    OccupiedSource::Folder(_) => "the folder",
                },
                occupied.free_name
            );

            dialog
                .title(title)
                .width(px(480.))
                .content(move |content, _, cx| {
                    let lead = lead.clone();
                    let where_from = where_from.clone();
                    let consequence = consequence.clone();
                    content.child(
                        v_flex()
                            .p_4()
                            .gap_2()
                            .child(div().text_sm().child(lead))
                            .children(where_from.map(|sentence| {
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(sentence)
                            }))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(consequence),
                            ),
                    )
                })
                .footer(
                    DialogFooter::new()
                        .p_4()
                        .child(
                            DialogClose::new()
                                .child(Button::new("cancel-occupied").outline().label("Cancel")),
                        )
                        .child({
                            let occupied = occupied.clone();
                            let this = this.clone();
                            Button::new("keep-both")
                                .outline()
                                .label("Keep both")
                                .on_click(move |_, window, cx| {
                                    let occupied = occupied.clone();
                                    this.update(cx, |this, cx| {
                                        this.retry_occupied(&occupied, false, window, cx);
                                    })
                                    .ok();
                                    window.close_dialog(cx);
                                })
                        })
                        .child({
                            let occupied = occupied.clone();
                            let this = this.clone();
                            Button::new("replace-installed")
                                .primary()
                                .label("Replace")
                                .on_click(move |_, window, cx| {
                                    let occupied = occupied.clone();
                                    this.update(cx, |this, cx| {
                                        this.retry_occupied(&occupied, true, window, cx);
                                    })
                                    .ok();
                                    window.close_dialog(cx);
                                })
                        }),
                )
        });
    }

    /// Where the copy already in the store came from, when the scan knows.
    ///
    /// Read from the scan that is already in memory rather than from the disk:
    /// the answer decides whether this is the same skill arriving twice or two
    /// different skills wanting one name, and it is worth nothing if getting it
    /// costs another wait.
    fn provenance_sentence(&self, occupied: &Occupied) -> Option<SharedString> {
        let scan = self.scan()?;
        let skill = scan
            .skills
            .iter()
            .find(|skill| skill.name == occupied.name)?;
        let provenance = skill.provenance.as_ref()?;
        let (owner, repo) = provenance.owner_repo()?;
        let OccupiedSource::Download(wanted) = &occupied.source else {
            // Nothing says where a picked folder came from, so the only fact
            // to hand is where the copy already in the store came from.
            return Some(format!("It came from {owner}/{repo}.").into());
        };
        let wanted = &wanted.repo;
        Some(if owner == wanted.owner && repo == wanted.repo {
            format!("It came from the same repository, {}.", wanted.slug()).into()
        } else {
            format!("It came from a different repository, {owner}/{repo}.").into()
        })
    }

    /// Try again the way the dialog was answered: replacing what is there, or
    /// keeping both under the free name the dialog offered.
    ///
    /// The two sources take the same two options — [`InstallOptions`] and
    /// [`ImportOptions`] each have `named` and `replacing` — so the dialog puts
    /// one question and this settles which call answers it.
    fn retry_occupied(
        &mut self,
        occupied: &Occupied,
        replace: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &occupied.source {
            OccupiedSource::Download(location) => {
                let (options, label) = if replace {
                    (InstallOptions::new().replacing(), occupied.name.clone())
                } else {
                    (
                        InstallOptions::new().named(occupied.free_name.clone()),
                        SharedString::from(occupied.free_name.clone()),
                    )
                };
                self.run_install(
                    InstallPlan::Ready(vec![location.clone()]),
                    options,
                    label,
                    None,
                    window,
                    cx,
                );
            }
            OccupiedSource::Folder(source) => {
                let options = if replace {
                    ImportOptions::new().replacing()
                } else {
                    ImportOptions::new().named(occupied.free_name.clone())
                };
                self.run_import(source.clone(), options, window, cx);
            }
        }
    }

    /// Ask for a repository, then download whatever it names.
    ///
    /// One field, because every spelling this accepts is one string. The forms
    /// are listed under it rather than split across several fields: a user who
    /// has a URL on the clipboard should be able to paste it.
    pub(crate) fn open_install_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.install_spec
            .update(cx, |state, cx| state.set_value("", window, cx));

        let field = self.install_spec.clone();
        let store = display_path(&self.roots.store_dir(), &self.roots);
        let this = cx.entity().downgrade();

        window.open_dialog(cx, move |dialog, _, _| {
            let field = field.clone();
            let store = store.clone();
            let this = this.clone();
            dialog
                .title("Install from GitHub")
                .width(px(460.))
                .content(move |content, _, cx| {
                    let store = store.clone();
                    content.child(
                        v_flex().p_4().gap_4().child(
                            v_flex()
                                .gap_2()
                                .child(Label::new("Repository"))
                                .child(Input::new(&field).small())
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(
                                            "owner/repo, owner/repo@branch, \
                                             owner/repo/path/to/skill, a github.com URL, or an \
                                             SSH remote.",
                                        ),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(format!(
                                            "Without a branch, Skillbase asks GitHub which one \
                                             the repository uses. A repository that holds several \
                                             skills asks which of them to install. The download \
                                             goes into {store}, where every agent that reads that \
                                             directory can see it."
                                        )),
                                ),
                        ),
                    )
                })
                .footer(
                    DialogFooter::new()
                        .p_4()
                        .child(
                            DialogClose::new()
                                .child(Button::new("cancel-install").outline().label("Cancel")),
                        )
                        .child(
                            Button::new("confirm-install")
                                .primary()
                                .label("Install")
                                .on_click(move |_, window, cx| {
                                    this.update(cx, |this, cx| this.install_from_spec(window, cx))
                                        .ok();
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    fn install_from_spec(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let spec = self.install_spec.read(cx).value().trim().to_string();
        let Some(parsed) = SkillLocation::parse_spec(&spec) else {
            window.push_notification(
                Notification::error(if spec.is_empty() {
                    "Name a repository first, as owner/repo or as a github.com URL.".to_string()
                } else {
                    format!(
                        "`{spec}` does not name a GitHub repository. Try owner/repo, or paste \
                         the repository's URL."
                    )
                })
                .title("Could not install")
                // What the user typed is still in the field, and this says
                // what is wrong with it. It stays until it is dismissed.
                .autohide(false),
                cx,
            );
            return;
        };
        let label = SharedString::from(parsed.location.dir_name().to_string());
        self.run_install(
            InstallPlan::Settle(Settle::Spec(parsed)),
            InstallOptions::new(),
            label,
            None,
            window,
            cx,
        );
    }

    /// Ask for a folder on this machine, and copy the skill in it into the
    /// store.
    ///
    /// The third way a skill gets here, beside Discover and a GitHub install: a
    /// skill someone sent over, or one in a repository that is already cloned.
    /// Without this the only route is to copy the directory into the store in
    /// Finder and press Refresh, which nothing in the interface says.
    ///
    /// The picker takes directories only. A skill is a folder, and the refusal
    /// for a file is a sentence rather than a thing the user can pick.
    pub(crate) fn import_from_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Install".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            // Three ways to end up with nothing: the picker failed to open, it
            // was cancelled, or it came back empty. None of them is worth a
            // notification — the user closed a dialog they opened.
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(source) = paths.into_iter().next() else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.run_import(source, ImportOptions::new(), window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Copy a picked folder into the store, and say what stands.
    ///
    /// The copy runs on a background task: nothing here reaches the network,
    /// but a skill that carries a few hundred megabytes of assets takes long
    /// enough that copying it on the main thread would stop the window.
    ///
    /// It writes into the same store a download does, so it waits for the same
    /// slot, and a name that is taken puts the same question to the user.
    fn run_import(
        &mut self,
        source: PathBuf,
        options: ImportOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The folder's own name, which is what the user picked. The skill's
        // real name comes out of its frontmatter and is not known until the
        // import has read it.
        let label = dir_name(&source).unwrap_or_else(|| "the folder".into());
        if let Some(sentence) = self.busy_sentence(&format!("install {label}")) {
            window.push_notification(Notification::info(sentence).title("Already running"), cx);
            return;
        }

        // Taken now, before anything that can take time: `exceeds` walks the
        // folder on a background thread, and while it walks there is no slot
        // to stop a download from starting and writing into the store
        // alongside this copy. The strip stays hidden until the copy turns out
        // to be slow, which is a separate question from holding the slot.
        let mut progress = Installing::new("copying", "copying the folder").hidden();
        progress.working_on(label.clone());
        let job = progress.id();
        self.installing = Some(progress);
        cx.notify();

        let roots = self.roots.clone();
        // Detached rather than held: an import cannot be stopped part-way, so
        // there is no handle worth keeping, and putting it in the slot a
        // download uses would let the next download drop it mid-copy.
        cx.spawn_in(window, async move |this, cx| {
            // A copy of a handful of text files is over before the window
            // could paint anything about it, and a strip that appears and
            // vanishes is noise. Measuring first is what keeps the strip for
            // the copies that actually make the user wait.
            let slow = cx
                .background_spawn({
                    let source = source.clone();
                    async move { exceeds(&source, PROGRESS_BYTES) }
                })
                .await;
            if slow {
                this.update(cx, |this, cx| {
                    if let Some(progress) = this.installing.as_mut().filter(|it| it.id() == job) {
                        progress.show();
                        cx.notify();
                    }
                })
                .ok();
            }

            let imported = cx
                .background_spawn({
                    let roots = roots.clone();
                    let source = source.clone();
                    async move { Installer::new(roots).import(&source, &options) }
                })
                .await;

            this.update_in(cx, |this, window, cx| {
                // Only this import's own slot. Clearing whatever is there
                // takes down another job's progress and puts its Cancel out of
                // reach while it goes on writing.
                if this.installing.as_ref().is_some_and(|it| it.id() == job) {
                    this.installing = None;
                }
                cx.notify();

                // The one refusal with something to offer. Every other one is
                // a sentence: `NotASkill` says what to pick instead, and
                // `AlreadyManaged` points at Adopt.
                if let Err(InstallError::AlreadyExists { path }) = &imported {
                    let name = dir_name(path).unwrap_or(label);
                    let free_name = free_name(&this.roots, &name);
                    let path = path.clone();
                    this.open_occupied_dialog(
                        Occupied {
                            source: OccupiedSource::Folder(source),
                            name,
                            path,
                            free_name,
                        },
                        window,
                        cx,
                    );
                    return;
                }

                let landed = imported.as_ref().ok().and_then(|(dir, _)| dir_name(dir));
                let changed = report(
                    "Installed",
                    "Could not install",
                    imported.map(|(_, outcome)| outcome),
                    &this.roots,
                    window,
                    cx,
                );
                match landed {
                    // The list is what shows the new skill, so the same
                    // rescan-and-select a download ends with.
                    Some(name) => this.installed(name, window, cx),
                    None if changed => this.rescan(None, window, cx),
                    None => {}
                }
            })
            .ok();
        })
        .detach();
    }

    /// Why the store cannot be written to right now, or `None` when it can.
    ///
    /// One job at a time, whichever view started it: they share GitHub's hourly
    /// budget, and two of them writing into `~/.agents/skills` at once is not
    /// something the store is asked to survive. `then` is what the user just
    /// asked for, written as the clause that ends the sentence — "install pdf",
    /// "update the rest" — so every refusal reads the same way round.
    pub(crate) fn busy_sentence(&self, then: &str) -> Option<String> {
        let busy = self.installing.as_ref().or(self.updating.as_ref())?;
        Some(if busy.stoppable() {
            format!(
                "Skillbase is already {}. Wait for it to finish, or cancel it, then {then}.",
                busy.doing()
            )
        } else {
            // A copy from a folder cannot be stopped part-way, so offering a
            // Cancel that is not there would be a lie.
            format!(
                "Skillbase is already {}. Wait for it to finish, then {then}.",
                busy.doing()
            )
        })
    }

    /// Settle what was asked for, download it, and say what stands.
    ///
    /// The lookups, the download, the extraction and the write into the store
    /// all happen on a background task; what lands on the main thread is a
    /// decision or a result. The task is held rather than detached, because a
    /// download the user cannot stop is a window that sits still for as long as
    /// an HTTP request is allowed to take.
    ///
    /// Several skills install one after another rather than at once. GitHub's
    /// hourly budget is shared between them, a repository archive is downloaded
    /// per skill, and one summary at the end reads better than one notification
    /// per skill stacked up the side of the window.
    fn run_install(
        &mut self,
        plan: InstallPlan,
        options: InstallOptions,
        label: SharedString,
        row: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Both refusals are the same refusal: one job writes into the store at
        // a time. `update_all` turns a Discover install away for the same
        // reason, so this has to turn a batch update away or the two run
        // together and the window grows a second progress strip.
        if let Some(sentence) = self.busy_sentence(&format!("install {label}")) {
            // The rest of the list stays live while one row downloads, so a
            // second click has to be answered rather than silently dropped.
            window.push_notification(Notification::info(sentence).title("Already running"), cx);
            return;
        }

        let cancel = Arc::new(AtomicBool::new(false));
        let total = match &plan {
            InstallPlan::Ready(locations) => locations.len(),
            InstallPlan::Settle(_) => 1,
        };
        let mut progress = Installing::new("downloading", "working out what to install")
            .of(total)
            .from_row(row)
            .cancelled_by(cancel.clone());
        progress.working_on(label);
        self.installing = Some(progress);
        cx.notify();

        let roots = self.roots.clone();
        let options = options.cancelled_by(cancel.clone());

        self.install_task = Some(cx.spawn_in(window, async move |this, cx| {
            // Step one: work out what to install, when that is not settled.
            let locations = match plan {
                InstallPlan::Ready(locations) => locations,
                InstallPlan::Settle(settle) => {
                    let settled = cx.background_spawn(async move { settle.run() }).await;
                    let locations = this
                        .update_in(cx, |this, window, cx| match settled {
                            Ok(Settled::One(location)) => {
                                if let Some(progress) = &mut this.installing {
                                    progress.working_on(location.dir_name().to_string());
                                }
                                cx.notify();
                                Some(vec![location])
                            }
                            Ok(Settled::Choose {
                                title,
                                lead,
                                locations,
                            }) => {
                                this.installing = None;
                                cx.notify();
                                this.open_location_dialog(title, lead, locations, window, cx);
                                None
                            }
                            Ok(Settled::Nothing(reason)) => {
                                this.installing = None;
                                cx.notify();
                                window.push_notification(
                                    Notification::error(reason)
                                        .title("Nothing to install")
                                        // Nothing was installed, so the list
                                        // looks exactly as it did before the
                                        // click. This sentence is the only
                                        // thing that says why.
                                        .autohide(false),
                                    cx,
                                );
                                None
                            }
                            Err(error) => {
                                this.installing = None;
                                cx.notify();
                                report_install(
                                    "Installed",
                                    "Could not install",
                                    Err(error),
                                    &this.roots,
                                    window,
                                    cx,
                                );
                                None
                            }
                        })
                        .ok()
                        .flatten();
                    match locations {
                        Some(locations) => locations,
                        None => return,
                    }
                }
            };

            // Step two: download them, one at a time.
            let total = locations.len();
            let mut results: Vec<(SharedString, Result<Installed, FetchError>)> = Vec::new();
            let mut occupied: Option<Occupied> = None;

            for (index, location) in locations.into_iter().enumerate() {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let name = SharedString::from(location.dir_name().to_string());
                let written = results.iter().filter(|(_, done)| done.is_ok()).count();
                this.update(cx, |this, cx| {
                    if let Some(progress) = &mut this.installing {
                        progress.working_on(name.clone());
                        progress.reached(index, written, total);
                    }
                    cx.notify();
                })
                .ok();

                let installed = cx
                    .background_spawn({
                        let roots = roots.clone();
                        let options = options.clone();
                        let location = location.clone();
                        async move { install_skill(&roots, &location, &options) }
                    })
                    .await;

                match installed {
                    Err(FetchError::Cancelled) => break,
                    // One skill whose name is taken is a decision, not a
                    // failure. In a batch it is one line of the summary,
                    // because forty dialogs in a row is not a decision anyone
                    // can make.
                    Err(FetchError::Install(InstallError::AlreadyExists { path }))
                        if total == 1 =>
                    {
                        // The name that is taken is the destination's, which
                        // came from the downloaded frontmatter and need not be
                        // the directory name in the repository.
                        let name = dir_name(&path).unwrap_or(name);
                        let free_name = free_name(&roots, &name);
                        occupied = Some(Occupied {
                            source: OccupiedSource::Download(location),
                            name,
                            path,
                            free_name,
                        });
                        break;
                    }
                    installed => results.push((name, installed)),
                }
            }

            this.update_in(cx, |this, window, cx| {
                this.installing = None;
                cx.notify();

                if let Some(occupied) = occupied {
                    this.open_occupied_dialog(occupied, window, cx);
                    return;
                }
                if total == 1 {
                    let Some((_, installed)) = results.pop() else {
                        return;
                    };
                    if let Some(name) = report_install(
                        "Installed",
                        "Could not install",
                        installed,
                        &this.roots,
                        window,
                        cx,
                    ) {
                        this.installed(name, window, cx);
                    }
                    return;
                }
                if let Some(first) = report_batch(&results, total, &this.roots, window, cx) {
                    this.installed(first, window, cx);
                }
            })
            .ok();
        }));
    }

    /// Stop the download, and say what stands.
    ///
    /// Dropping the task is what makes the window answer straight away: the
    /// blocking call cannot be interrupted, but the flag is set first, and the
    /// install reads it before it writes anything into the store.
    ///
    /// What the notification says is what this side knows: the install stopped,
    /// and how many of a batch had been counted as installed before it did.
    /// Not what is in the store. The count comes from the last update the task
    /// sent, so a skill whose write finished as the flag was set is on disk
    /// without having been counted, and a sentence about the store would then
    /// be wrong. The scan that follows is what answers that question, and it
    /// runs whatever the count says.
    fn cancel_install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(progress) = self.installing.take() else {
            return;
        };
        progress.stop();
        self.install_task = None;
        cx.notify();

        let done = progress.done();
        let total = progress.total();
        window.push_notification(
            // One skill was asked for, so a count of one says nothing its name
            // does not.
            Notification::info(if total > 1 {
                format!("{done} of {total} skills were installed before it stopped.")
            } else {
                format!(
                    "Skillbase stopped installing {}.",
                    progress
                        .name()
                        .cloned()
                        .unwrap_or_else(|| "the skill".into())
                )
            })
            .title("Install cancelled"),
            cx,
        );
        // A scan reads the store, which is the only thing that can say what is
        // in it. It costs one directory walk, and skipping it on a count of
        // zero is what left the list disagreeing with the disk.
        self.rescan(None, window, cx);
    }
}

impl Settle {
    /// Ask GitHub where the request actually points.
    ///
    /// Blocking, and it makes network requests, so callers run it on a
    /// background task.
    fn run(self) -> Result<Settled, FetchError> {
        let github = GitHub::from_env(UreqHttp::new());
        match self {
            Settle::Hit(hit) => {
                let name = SharedString::from(hit.name.clone());
                let source = hit.source.clone();
                let locations = resolve(&github, &hit, None)?;
                Ok(match locations.len() {
                    0 => Settled::Nothing(
                        format!(
                            "{source} holds no directory called {name} with a SKILL.md in it. \
                             The registry and the repository disagree, usually because it was \
                             renamed or removed upstream."
                        )
                        .into(),
                    ),
                    1 => Settled::One(locations.into_iter().next().expect("one location")),
                    count => Settled::Choose {
                        title: format!("Which {name}?").into(),
                        lead: format!(
                            "{source} holds {count} directories called {name}. Tick the one to \
                             download."
                        )
                        .into(),
                        locations,
                    },
                })
            }
            Settle::Spec(parsed) => {
                let mut location = parsed.location;
                // A spelling that names no ref is not a spelling that means
                // `main`. Guessing it is what makes a repository on `master`
                // fail with a branch the user never typed.
                if !parsed.explicit_ref {
                    location.repo.reference =
                        github.default_branch(&location.repo.owner, &location.repo.repo)?;
                }
                if !location.path.is_empty() {
                    return Ok(Settled::One(location));
                }

                // A whole repository was named. It is usually one skill at its
                // root, and sometimes forty of them one directory down.
                let slug = location.repo.slug();
                let commit = github.ref_sha(&location.repo)?;
                let dirs = github.list_skill_dirs(&location.repo, &commit)?;
                Ok(match dirs.len() {
                    0 => Settled::Nothing(
                        format!(
                            "{slug} holds no SKILL.md anywhere on {}, so there is no skill in it \
                             to install.",
                            location.repo.reference
                        )
                        .into(),
                    ),
                    _ if dirs.iter().any(|dir| dir.is_empty()) => Settled::One(location),
                    1 => Settled::One(SkillLocation::new(
                        location.repo.clone(),
                        dirs.into_iter().next().expect("one directory"),
                    )),
                    count => Settled::Choose {
                        title: slug.clone().into(),
                        lead: format!(
                            "{slug} holds {count} skills, none at its root. Tick the ones to \
                             install."
                        )
                        .into(),
                        locations: dirs
                            .into_iter()
                            .map(|path| SkillLocation::new(location.repo.clone(), path))
                            .collect(),
                    },
                })
            }
        }
    }
}

/// One notification for a batch, rather than one per skill.
///
/// Returns the first skill that landed, for the list to select. A batch that
/// installed nothing selects nothing.
fn report_batch(
    results: &[(SharedString, Result<Installed, FetchError>)],
    total: usize,
    roots: &Roots,
    window: &mut Window,
    cx: &mut gpui_kit::App,
) -> Option<SharedString> {
    let installed: Vec<&SharedString> = results
        .iter()
        .filter(|(_, result)| result.is_ok())
        .map(|(name, _)| name)
        .collect();
    let failures: Vec<String> = results
        .iter()
        .filter_map(|(name, result)| {
            result
                .as_ref()
                .err()
                .map(|error| format!("{name}: {error}"))
        })
        .collect();

    let store = display_path(&roots.store_dir(), roots);
    let mut message = if installed.is_empty() {
        format!("None of the {total} skills were installed.")
    } else {
        format!("{} of {total} skills are now in {store}.", installed.len())
    };
    for failure in failures.iter().take(NAMED_FAILURES) {
        message.push('\n');
        message.push_str(failure);
    }
    if failures.len() > NAMED_FAILURES {
        message.push_str(&format!(
            "\nand {} more that did not install.",
            failures.len() - NAMED_FAILURES
        ));
    }

    let first = installed.first().map(|name| (*name).clone());
    let notification = if failures.is_empty() {
        Notification::success(message).title("Installed")
    } else if installed.is_empty() {
        Notification::error(message)
            .title("Could not install")
            .autohide(false)
    } else {
        Notification::warning(message)
            .title(format!("Installed {} of {total}", installed.len()))
            .autohide(false)
    };
    window.push_notification(notification, cx);
    // Every skill in the batch swept the same staging directory, so the first
    // sweep that could not clear it says what all of them would say. Without
    // this line a part-downloaded skill sits in `~/.skillbase/staging` with
    // nothing in the interface naming it.
    if let Some(warning) = results
        .iter()
        .filter_map(|(_, result)| result.as_ref().ok())
        .find_map(|installed| installed.staging.warning())
    {
        window.push_notification(
            Notification::warning(warning)
                .title("Staging directory not cleared")
                .autohide(false),
            cx,
        );
    }
    first
}

/// A directory's own name: the name in the way when a destination is occupied,
/// and the name a skill landed under when one was written.
fn dir_name(path: &Path) -> Option<SharedString> {
    Some(path.file_name()?.to_string_lossy().into_owned().into())
}

/// How much a folder has to hold before the window says anything about copying
/// it.
///
/// A skill is usually a few text files and is copied between two frames; a
/// strip for that appears and vanishes before it can be read. Past this much
/// the copy takes long enough that saying nothing would read as a window that
/// has stopped.
const PROGRESS_BYTES: u64 = 8 * 1024 * 1024;

/// Whether the tree under `dir` holds more than `limit` bytes.
///
/// It stops as soon as it knows, so the answer costs a walk rather than a
/// measurement. `.git` is skipped because the import skips it too, and a
/// checkout's history is routinely larger than the skill in it. Symlinks are
/// counted as the links they are: `DirEntry::metadata` does not follow one, and
/// neither does the copy.
fn exceeds(dir: &Path, limit: u64) -> bool {
    let mut total: u64 = 0;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_name() == ".git" {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                pending.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
                if total > limit {
                    return true;
                }
            }
        }
    }
    false
}

/// The first `name-2`, `name-3`, … that no directory in the store answers to.
fn free_name(roots: &Roots, name: &str) -> String {
    (2..u32::MAX)
        .map(|suffix| format!("{name}-{suffix}"))
        .find(|candidate| fs::symlink_metadata(roots.store_dir().join(candidate)).is_err())
        .unwrap_or_else(|| format!("{name}-2"))
}

/// How a location reads in the choice dialog: its path in the repository, which
/// is the only thing that tells two of them apart.
fn choice_label(location: &SkillLocation) -> SharedString {
    if location.path.is_empty() {
        "the repository root".into()
    } else {
        location.path.clone().into()
    }
}

/// What to name in the progress strip before the first download starts.
fn install_label(locations: &[SkillLocation]) -> SharedString {
    match locations.first() {
        Some(location) => location.dir_name().to_string().into(),
        None => "the skill".into(),
    }
}

/// A stable identity for a search result row.
///
/// The registry's own row id where there is one, and the repository and skill
/// id where there is not — never the row's position, which changes with every
/// keystroke.
fn hit_id(hit: &SearchHit) -> SharedString {
    if !hit.id.is_empty() {
        return hit.id.clone().into();
    }
    format!("{}/{}", hit.source, hit.skill_id).into()
}

/// How many installs the registry has seen, with the digits grouped so two
/// rows can be compared at a glance.
fn installs(count: u64) -> SharedString {
    let digits = count.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("{grouped} install{}", if count == 1 { "" } else { "s" }).into()
}

fn empty_state(
    title: &'static str,
    detail: SharedString,
    action: Option<AnyElement>,
    cx: &mut Context<Skillbase>,
) -> AnyElement {
    v_flex()
        .py_8()
        .gap_1()
        // The title carries the weight rather than a larger size: at this size
        // the explanation under it is the same `text_sm`, and without the
        // weight the two lines read as one paragraph with no heading.
        .child(div().text_sm().font_medium().child(title))
        .child(
            div()
                .max_w(rems(32.))
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(detail),
        )
        .children(action.map(|action| h_flex().pt_2().child(action)))
        .into_any_element()
}

impl Skillbase {
    /// Give the work area to Discover.
    pub(crate) fn show_discover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show(WorkArea::Discover, cx);
        self.discover_query
            .update(cx, |state, cx| state.focus(window, cx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of this test's own, named after it, so two tests running at
    /// once cannot write over one another.
    fn scratch(what: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "skillbase-{what}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    #[test]
    fn an_install_count_groups_its_digits() {
        assert_eq!(installs(0), "0 installs");
        assert_eq!(installs(1), "1 install");
        assert_eq!(installs(999), "999 installs");
        assert_eq!(installs(1_204), "1,204 installs");
        assert_eq!(installs(1_000_000), "1,000,000 installs");
    }

    #[test]
    fn a_row_is_identified_by_the_registry_and_never_by_its_position() {
        let mut hit = SearchHit {
            id: "row-7".into(),
            skill_id: "pdf".into(),
            name: "PDF".into(),
            installs: 3,
            source: "anthropics/skills".into(),
        };
        assert_eq!(hit_id(&hit), "row-7");
        hit.id = String::new();
        assert_eq!(hit_id(&hit), "anthropics/skills/pdf");
    }

    #[test]
    fn a_choice_names_its_path_and_calls_the_root_by_name() {
        let repo = skillbase_core::RepoRef::new("anthropics", "skills", "main");
        assert_eq!(
            choice_label(&SkillLocation::new(repo.clone(), "document-skills/pdf")),
            "document-skills/pdf"
        );
        assert_eq!(
            choice_label(&SkillLocation::new(repo, "")),
            "the repository root"
        );
    }

    #[test]
    fn a_job_says_what_it_is_doing_in_a_sentence_and_at_the_head_of_one() {
        let mut job = Installing::new("downloading", "working out what to install");
        assert_eq!(job.doing(), "working out what to install");
        assert_eq!(job.label(), "Working out what to install");
        assert_eq!(job.count(), None);

        job.working_on("pdf");
        assert_eq!(job.doing(), "downloading pdf");
        assert_eq!(job.label(), "Downloading pdf");

        job.reached(2, 2, 5);
        assert_eq!(job.count().as_deref(), Some("3 of 5"));
        assert_eq!(job.done(), 2);
        assert_eq!(job.total(), 5);
        // Nothing to set, so the strip offers no Cancel.
        assert!(!job.stoppable());
    }

    #[test]
    fn a_job_holds_the_slot_whether_or_not_the_strip_shows_it() {
        // A download says what it is doing from the first frame.
        assert!(Installing::new("downloading", "working out what to install").shown());
        // A copy from a folder holds the slot while it is measured, and only
        // asks for the strip once it turns out to be slow.
        let mut copy = Installing::new("copying", "copying the folder").hidden();
        assert!(!copy.shown());
        copy.show();
        assert!(copy.shown());
    }

    #[test]
    fn each_job_is_told_apart_from_the_next_one_to_hold_the_slot() {
        let first = Installing::new("copying", "copying the folder");
        let second = Installing::new("downloading", "working out what to install");
        assert_eq!(first.id(), first.id());
        assert_ne!(first.id(), second.id());
    }

    #[test]
    fn a_folder_is_measured_only_until_the_answer_is_known_and_never_counts_git() {
        let dir = scratch("exceeds");
        fs::create_dir_all(dir.join(".git")).expect("a checkout's bookkeeping");
        fs::write(dir.join(".git/pack"), vec![0u8; 512]).expect("pack");
        fs::write(dir.join("SKILL.md"), vec![0u8; 100]).expect("the skill");
        // The 512 bytes under `.git` are not part of the skill and are not
        // copied, so they cannot decide whether the copy is worth a strip.
        assert!(!exceeds(&dir, 200));
        fs::create_dir_all(dir.join("assets")).expect("assets");
        fs::write(dir.join("assets/big"), vec![0u8; 400]).expect("a large asset");
        assert!(exceeds(&dir, 200));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_second_copy_takes_the_first_free_numbered_name() {
        let dir = scratch("free-name");
        let roots = Roots::new(dir.clone());
        fs::create_dir_all(roots.store_dir()).expect("store");
        assert_eq!(free_name(&roots, "pdf"), "pdf-2");
        fs::create_dir_all(roots.store_dir().join("pdf-2")).expect("pdf-2");
        assert_eq!(free_name(&roots, "pdf"), "pdf-3");
        fs::remove_dir_all(&dir).ok();
    }
}
