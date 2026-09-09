//! The application view: three columns, each opening with its own header band.
//!
//! This view owns the one [`Roots`] every read and every write resolves
//! against, the scan that comes back from it, the selection and the search
//! field. The detail pane owns editing and the mutations that follow from it,
//! and asks for a re-scan when it has changed the disk.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::resizable::{
    ResizablePanelEvent, ResizableState, h_resizable, resizable_panel,
};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Root, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription, Task, Window,
    div, px,
};
use skillbase_core::{
    AgentDef, FetchError, GITHUB_TOKEN_ENV, GitHub, InstallError, InstallOptions, Installed,
    Installer, LocalState, Outcome, Provenance, RateLimit, RemoteCache, RepoRef, Roots,
    SkillLocation, TokenSource, UpdateReport, UpdateStatus, UreqHttp, Usage, check_updates,
    github_token_source, local_state, refresh_github_token,
};

use crate::menus::{
    CheckForUpdates, ClearMarks, CloseWindow, DeleteSkill, FindSkill, InstallFromGitHub,
    LinkMarked, MarkAll, NewSkill, Quit, ReloadSkills, RevealInFinder, Save, ShowDiscover,
    ShowSettings, ToggleSidebar, UnlinkMarked, UpdateSkill,
};
use crate::ui::detail::{DetailEvent, DetailPane, Proceed};
use crate::ui::discover::{Described, InstallChoice, Installing, SearchState, StartedFrom};
use crate::ui::list::{LIST_MAX_WIDTH, LIST_MIN_WIDTH, LIST_WIDTH, Marks};
use crate::ui::model::{
    Library, LoadedPreferences, Preferences, Scan, Scope, SkillSort, SkillView, in_words,
    resolve_roots,
};
use crate::ui::{
    BAND_HEIGHT, DETAIL_MIN_WIDTH, Notice, TRAFFIC_LIGHT_INSET, cache_failure_notification,
    cannot_see, capitalized, drag_band, install_skill, install_warnings, link_label, push_notice,
    reach_sentence, report, take_cache_failure, wrote_sentence,
};

/// Where the scan has got to. A scan of every scope on a busy machine takes
/// long enough that the first paint must not wait for it.
pub(crate) enum ScanState {
    Loading,
    Ready(Rc<Scan>),
    Failed(SharedString),
}

/// Which view has the work area — the two panes to the right of the sidebar.
///
/// One value rather than a flag per destination, so two of them cannot be true
/// at once. Skills leaves the scope and the selection exactly where they were,
/// which is what lets Settings and Discover come back to the same skill.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WorkArea {
    /// The skill list and the detail pane.
    Skills,
    /// The skills.sh search.
    Discover,
    Settings,
}

/// Where the update check has got to.
///
/// A check is a few HTTP requests against a budget of sixty an hour, so it
/// runs once after the first scan and then only when the user asks. Until one
/// lands, no skill claims to be up to date and none claims otherwise.
pub(crate) enum UpdateState {
    /// Nothing has been checked yet.
    Idle,
    Checking,
    Ready(Rc<UpdateReport>),
}

/// How many names a summary spells out before it counts the rest.
const NAMED_IN_SUMMARY: usize = 3;

/// One skill a batch update would replace.
struct UpdateCandidate {
    name: SharedString,
    /// Where to download the replacement from.
    location: SkillLocation,
    /// The directory on disk, hashed to tell an edited copy from an untouched
    /// one.
    dir: PathBuf,
}

/// The root view.
pub struct Skillbase {
    /// Every path the application reads or writes hangs off this one value.
    pub(crate) roots: Roots,
    /// True when `SKILLBASE_HOME` pointed the application somewhere other than
    /// the real home. Said out loud beside the application's name, because
    /// every mutation lands there.
    pub(crate) home_overridden: bool,
    pub(crate) scan: ScanState,
    pub(crate) scope: Scope,
    pub(crate) selected: Option<SharedString>,
    /// The rows the bulk actions work on.
    ///
    /// Beside the selection rather than instead of it: `selected` is the one
    /// skill the detail pane has open, and that stays true however many rows
    /// are marked. A plain click sets both to the same row, so a marked set of
    /// one is the interface that was here before.
    pub(crate) marks: Marks,
    /// True while a bulk write is running.
    ///
    /// One at a time: the band's buttons go off while it runs, so a second
    /// Delete cannot be sent against a set the first one is still removing.
    pub(crate) bulk_busy: bool,
    pub(crate) sidebar_collapsed: bool,
    /// Whether the Agents group in the sidebar is closed. Closed on launch:
    /// the Library rows are the usual destination, and a dozen agent names
    /// under them is a list the user opens when they need it.
    pub(crate) agents_collapsed: bool,
    /// Which view has the work area.
    pub(crate) work_area: WorkArea,
    /// What the user chose last time. Read once at startup and written back on
    /// every change.
    ///
    /// The settings that have a live field of their own — the scope, the
    /// selection, the two collapse flags — are copied into this record by
    /// [`Skillbase::sync_preferences`], which runs immediately before every
    /// write. Nothing else copies them: a mirror kept by each setter is one a
    /// new setter can forget, and a scope written without its selection opens
    /// the next launch on a list that does not hold the skill in the detail
    /// pane. The live field stays the one the interface reads.
    pub(crate) preferences: Preferences,
    pub(crate) search: Entity<InputState>,
    /// How many times each skill has been invoked, from the session records
    /// the agents that keep them leave behind. `None` until the count lands,
    /// which is a few hundred milliseconds after the window opens.
    pub(crate) usage: Option<Rc<Usage>>,
    /// The New skill dialog's two fields. Held here rather than rebuilt each
    /// time the dialog opens, because the dialog's content builder is called on
    /// every frame it is on screen.
    pub(crate) new_name: Entity<InputState>,
    pub(crate) new_description: Entity<TextareaState>,
    /// The Install from GitHub dialog's one field, held here for the same
    /// reason as the two above.
    pub(crate) install_spec: Entity<InputState>,
    /// The download or the folder copy running now, or `None` when neither is.
    ///
    /// Held rather than a flag so the interface can name the skill it is
    /// waiting for, disable that one Discover row rather than every row, and
    /// offer a way to stop it.
    pub(crate) installing: Option<Installing>,
    /// The batch update in flight, or `None` when none is. The same type as
    /// `installing`, and in its own slot rather than sharing that one because
    /// the two are started from different views; only one of them can run at a
    /// time, and each refuses to start while the other is running.
    pub(crate) updating: Option<Installing>,
    /// The skills a repository offered, and which of them are ticked.
    ///
    /// Owned here rather than by the dialog because a dialog's builder runs
    /// again on every frame and keeps nothing between them.
    pub(crate) install_choices: Vec<InstallChoice>,
    /// Which view opened the chooser, so that installing what is ticked lands
    /// where the install that raised it would have landed. A repository holding
    /// several skills asks this question from Discover and from the Install
    /// from GitHub dialog alike.
    pub(crate) install_choices_from: StartedFrom,
    /// The Discover pane's search field.
    pub(crate) discover_query: Entity<InputState>,
    /// What skills.sh last said, or why it did not say anything.
    pub(crate) discover: SearchState,
    /// Bumped on every keystroke, so a search that lands after a newer one was
    /// typed is dropped rather than shown.
    pub(crate) discover_generation: u64,
    /// The result row the user has opened, if any. One at a time: opening a row
    /// asks GitHub where the skill is, and a page of them opened at once would
    /// spend the hourly budget on rows nobody read.
    pub(crate) discover_open: Option<SharedString>,
    /// What opening a row found, kept by row id for as long as the results
    /// stand, so closing and reopening one does not ask again.
    pub(crate) discover_details: HashMap<SharedString, Described>,
    /// Where the GitHub token came from at startup, once the lookup has
    /// finished. Read once, because neither the environment nor a `gh` login is
    /// watched for changes.
    ///
    /// `None` while it is still being read. Finding it means stat-ing every
    /// entry on `PATH` and then waiting on `gh auth token`, which on a machine
    /// with a locked keychain takes long enough that the first frame must not
    /// wait for it. Nothing on that frame needs the answer.
    pub(crate) token_source: Option<TokenSource>,
    /// What the last update check found.
    pub(crate) updates: UpdateState,
    /// What GitHub last said about the request budget, so a spent limit can be
    /// reported as a wait rather than as a blank failure.
    pub(crate) rate_limit: Option<RateLimit>,
    /// Set when the scan now in flight should be followed by an update check.
    ///
    /// Not every scan: a scan follows every save and every visibility toggle,
    /// and asking GitHub about thirteen repositories each time would spend an
    /// unauthenticated hour's budget on a morning's editing. The check runs
    /// after the first scan, after Refresh, and after an install or an update.
    check_after_scan: bool,
    /// The skill the scan now in flight should land on, or `None` to keep
    /// whatever is selected when it arrives.
    ///
    /// Held here rather than carried by the scan task so that a selection made
    /// while the scan is running wins: without it, saving and then switching
    /// skills would bounce back to the one that was saved.
    pending_select: Option<SharedString>,
    /// Set when the scan now in flight should be followed by opening the detail
    /// pane's "Visible to" section. An install is the one moment when which
    /// agents get the skill is the next question, and the section is closed on
    /// every selection.
    open_visibility_after_scan: bool,
    detail: Entity<DetailPane>,
    panes: Entity<ResizableState>,
    /// Focus for the window as a whole, held so the menu bar's commands have
    /// somewhere to land.
    ///
    /// An action handler is only reachable along the path from the dispatch
    /// tree's root to whatever has focus. With nothing focused that path is
    /// the root alone, every menu item that resolves to this view is drawn
    /// greyed out, and its shortcut does nothing. Focusing the outermost
    /// element puts this view on the path from the first frame, and it stays
    /// there afterwards because everything else that takes focus sits inside
    /// it.
    focus: FocusHandle,
    /// Bumped on every scan so a result that arrives after a newer request is
    /// dropped rather than applied.
    generation: u64,
    /// The pending write of the settings file, or `None` when nothing is
    /// waiting to be written.
    ///
    /// Dropping the task cancels it, which is how [`Skillbase::save_settings`]
    /// debounces: a drag that moves the window fires an observer on every
    /// frame, and only the last one should reach the disk.
    _settle_task: Option<Task<()>>,
    /// The one-off task that reports a settings file that could not be read.
    /// Held only so it is not dropped before it runs.
    _notice_task: Option<Task<()>>,
    _scan_task: Option<Task<()>>,
    _usage_task: Option<Task<()>>,
    _token_task: Option<Task<()>>,
    _updates_task: Option<Task<()>>,
    pub(crate) _discover_task: Option<Task<()>>,
    /// The read of one result row's SKILL.md. Held so that opening another row
    /// drops the one before it.
    pub(crate) _describe_task: Option<Task<()>>,
    /// The install now in flight. Kept rather than detached, because Cancel
    /// needs something to drop.
    pub(crate) install_task: Option<Task<()>>,
    /// Repaints the progress strip while a job runs, so the strip can say how
    /// long the download has been going. Dropped when nothing is running.
    pub(crate) _progress_task: Option<Task<()>>,
    /// The batch update now in flight, kept for the same reason.
    update_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Skillbase {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Without a home there is nothing to scan, but the window should still
        // open and say why it is empty rather than refusing to start.
        let resolved = resolve_roots();
        let (roots, home_overridden) = match &resolved {
            Ok((roots, overridden)) => (roots.clone(), *overridden),
            Err(_) => (Roots::new(std::path::PathBuf::from("/")), false),
        };

        // One small file, read once, before the first paint. A background hop
        // would make the sidebar's first frame disagree with the preference and
        // then jump.
        let LoadedPreferences {
            preferences,
            problem,
        } = Preferences::load(&roots);

        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search skills"));
        let new_name = cx.new(|cx| InputState::new(window, cx).placeholder("my-new-skill"));
        let new_description = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("What the skill does, and when an agent should load it.")
        });
        let install_spec =
            cx.new(|cx| InputState::new(window, cx).placeholder("owner/repo/path/to/skill"));
        let discover_query =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search skills.sh"));
        let detail = cx.new(|cx| DetailPane::new(roots.clone(), window, cx));
        let panes = cx.new(|_| ResizableState::default());

        let focus = cx.focus_handle();
        focus.focus(window, cx);

        let subscriptions = vec![
            // Typing in the search field changes what the list shows, and a
            // marked set describes rows that are no longer the rows on screen.
            cx.subscribe(&search, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.reset_marks();
                    cx.notify();
                }
            }),
            // Typing in the Discover field starts a search once the typing
            // stops. Debounced in `search_registry`, not here.
            cx.subscribe_in(
                &discover_query,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.search_registry(window, cx);
                    }
                },
            ),
            cx.subscribe_in(&detail, window, Self::on_detail_event),
            // The split between the list and the detail pane. The event lands
            // once, on mouse-up, so this is the drag the user finished rather
            // than every pixel of it.
            cx.subscribe_in(
                &panes,
                window,
                |this, state, _: &ResizablePanelEvent, window, cx| {
                    let width = state.read(cx).sizes().first().map(|w| f32::from(*w));
                    if this.preferences.list_width == width {
                        return;
                    }
                    this.preferences.list_width = width;
                    this.save_settings(window, cx);
                },
            ),
            // Moving or resizing the window fires this on every frame of the
            // drag, which is why the write below is debounced.
            cx.observe_window_bounds(window, |this, window, cx| {
                let frame = crate::window_frame(window.window_bounds().get_bounds());
                // The frame is measured from the corner of whichever display
                // the window is on, so dragging it to another display changes
                // where those numbers point without changing the numbers.
                let display = crate::window_display(window, cx);
                if this.preferences.window == Some(frame) && this.preferences.display == display {
                    return;
                }
                this.preferences.window = Some(frame);
                this.preferences.display = display;
                this.save_settings(window, cx);
            }),
        ];

        let mut this = Self {
            roots,
            home_overridden,
            scan: ScanState::Loading,
            scope: preferences.scope,
            // The skill that was open last time, if it is still there. A scan
            // that cannot find it falls back to the first skill this scope
            // lists, which is what `apply_scan` already does for a skill
            // deleted while the window was open.
            selected: preferences.selected.clone().map(SharedString::from),
            marks: Marks::default(),
            bulk_busy: false,
            sidebar_collapsed: preferences.sidebar_collapsed,
            agents_collapsed: preferences.agents_collapsed,
            // Not restored. Settings and Discover are errands, not places to
            // live: coming back to Discover with yesterday's search still in
            // it would hide the library the user opened Skillbase to see.
            work_area: WorkArea::Skills,
            preferences,
            search,
            usage: None,
            new_name,
            new_description,
            install_spec,
            installing: None,
            updating: None,
            install_choices: Vec::new(),
            install_choices_from: StartedFrom::Elsewhere,
            discover_query,
            discover: SearchState::Idle,
            discover_generation: 0,
            discover_open: None,
            discover_details: HashMap::new(),
            token_source: None,
            updates: UpdateState::Idle,
            rate_limit: None,
            check_after_scan: true,
            pending_select: None,
            open_visibility_after_scan: false,
            detail,
            panes,
            focus,
            generation: 0,
            _settle_task: None,
            _notice_task: None,
            _scan_task: None,
            _usage_task: None,
            _token_task: None,
            _updates_task: None,
            _discover_task: None,
            _describe_task: None,
            install_task: None,
            _progress_task: None,
            update_task: None,
            _subscriptions: subscriptions,
        };
        // The red traffic light closes the window without passing through
        // `close_window`, so this is the only place that hears about it. The
        // debounced write goes with the view when it is dropped.
        let closing = cx.weak_entity();
        window.on_window_should_close(cx, move |_, cx| {
            closing.update(cx, |this, _| this.flush_settings()).ok();
            true
        });

        this.report_settings_problem(problem, window, cx);
        this.read_token_source(window, cx);
        match resolved {
            Ok(_) => {
                this.rescan(None, window, cx);
                this.count_usage(window, cx);
            }
            Err(error) => this.scan = ScanState::Failed(error.to_string().into()),
        }
        this
    }

    /// Find out where a GitHub token would come from, off the painting thread.
    ///
    /// The lookup stats every entry on `PATH` looking for `gh` and then runs
    /// `gh auth token`, which reads the keychain. On a machine where that is
    /// slow or locked it blocks for seconds, and doing it inline would hold up
    /// the first frame: a bouncing dock icon and no window. Nothing on that
    /// frame reads the answer, so it lands when it lands.
    fn read_token_source(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._token_task = Some(cx.spawn_in(window, async move |this, cx| {
            let source = cx
                .background_spawn(async move { github_token_source() })
                .await;
            this.update(cx, |this, cx| {
                this.token_source = Some(source);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Look for a GitHub token again, and say what turned up.
    ///
    /// The startup lookup runs once, so a user who hits a private-repository
    /// 404, logs in with `gh` in another window and comes back would otherwise
    /// go on sending whatever was found before the login until the application
    /// was relaunched. This is the way out of that without one.
    ///
    /// `token_source` goes back to `None` while it runs, which is the state the
    /// section already renders as "still looking", so the control disables
    /// itself and the caption stays true. The lookup itself waits on a `gh`
    /// subprocess, so it happens on a background thread.
    pub(crate) fn refresh_token_source(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.token_source.is_none() {
            return;
        }
        self.token_source = None;
        cx.notify();

        self._token_task = Some(cx.spawn_in(window, async move |this, cx| {
            let source = cx
                .background_spawn(async move { refresh_github_token() })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.token_source = Some(source);
                cx.notify();
                window.push_notification(token_notification(source), cx);
            })
            .ok();
        }));
    }

    /// Walk the filesystem again and adopt the result.
    ///
    /// `select` names the skill to land on once the scan arrives; `None` keeps
    /// whatever is selected if it is still there.
    pub(crate) fn rescan(
        &mut self,
        select: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.generation += 1;
        let generation = self.generation;
        let roots = self.roots.clone();
        self.pending_select = select;
        if matches!(self.scan, ScanState::Failed(_)) {
            self.scan = ScanState::Loading;
        }
        cx.notify();

        self._scan_task = Some(cx.spawn_in(window, async move |this, cx| {
            // The scan opens a few dozen directories and parses every SKILL.md
            // it finds. That does not belong on the thread that paints.
            let scan = cx.background_spawn(async move { Scan::load(&roots) }).await;
            this.update_in(cx, |this, window, cx| {
                if this.generation != generation {
                    return;
                }
                this.apply_scan(Rc::new(scan), window, cx);
            })
            .ok();
        }));
    }

    fn apply_scan(&mut self, scan: Rc<Scan>, window: &mut Window, cx: &mut Context<Self>) {
        // Keep the selection when the skill survived the change; otherwise fall
        // back to the first skill the current scope lists.
        let wanted = self.pending_select.take().or_else(|| self.selected.clone());
        let scope = self.scope;
        let selected = wanted
            .filter(|name| scan.get(name).is_some_and(|skill| self.lists(scope, skill)))
            .or_else(|| {
                scan.skills
                    .iter()
                    .find(|skill| self.lists(scope, skill))
                    .map(|skill| skill.name.clone())
            });

        self.selected = selected.clone();
        // A marked skill that the scan no longer finds, or that this scope no
        // longer lists, is a name in the band's count that nothing backs up.
        // A set that falls below two rows is no set at all, so it comes back to
        // whatever the detail pane is showing.
        let kept: Vec<SharedString> = self
            .marks
            .names()
            .iter()
            .filter(|name| scan.get(name).is_some_and(|skill| self.lists(scope, skill)))
            .cloned()
            .collect();
        self.marks.retain(move |name| kept.contains(name));
        if self.marks.len() < 2 {
            self.marks.reset_to(selected.clone());
        }
        self.scan = ScanState::Ready(scan.clone());
        self.detail
            .update(cx, |detail, cx| detail.show(scan, selected, window, cx));
        // After `show`, which closes the section on every change of skill.
        if std::mem::take(&mut self.open_visibility_after_scan) {
            self.detail
                .update(cx, |detail, cx| detail.open_visibility(cx));
        }
        self.push_updates(cx);
        if std::mem::take(&mut self.check_after_scan) {
            self.check_for_updates(window, cx);
        }
        cx.notify();
    }

    /// Hand the detail pane whatever the last check found.
    ///
    /// The pane renders one skill's status; the report covers every skill and
    /// arrives long after the scan, so it is pushed rather than read across.
    fn push_updates(&self, cx: &mut Context<Self>) {
        let report = match &self.updates {
            UpdateState::Ready(report) => Some(report.clone()),
            _ => None,
        };
        self.detail
            .update(cx, |detail, cx| detail.set_updates(report, cx));
    }

    fn on_detail_event(
        &mut self,
        _: &Entity<DetailPane>,
        event: &DetailEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            DetailEvent::Changed { select } => self.rescan(select.clone(), window, cx),
            // The skill was replaced from upstream, so its recorded sha moved
            // and the last report's answer for it is now wrong. Ask again.
            DetailEvent::Updated { select } => {
                self.check_after_scan = true;
                self.rescan(select.clone(), window, cx);
            }
        }
    }

    /// The status of one skill, when a check has run and covered it.
    pub(crate) fn update_status(&self, name: &str) -> Option<&UpdateStatus> {
        match &self.updates {
            UpdateState::Ready(report) => report.status(name),
            _ => None,
        }
    }

    /// Whether `scope` lists this skill.
    ///
    /// Every decision about whether a selection survives a scan or a change of
    /// scope goes through here, because [`Library::Updates`] is a list of the
    /// one thing the scan cannot answer: without the check's answer that group
    /// claims every skill, and the detail pane keeps a skill the list on screen
    /// does not hold.
    fn lists(&self, scope: Scope, skill: &SkillView) -> bool {
        scope.shows_with(skill, &|name| self.has_update(name))
    }

    /// True when this skill's row should carry an update marker.
    ///
    /// `NoBaseline` counts: it means the skill was installed by something that
    /// recorded no sha, so Skillbase cannot say it is current, and marking it
    /// is the honest half of that.
    pub(crate) fn has_update(&self, name: &str) -> bool {
        matches!(
            self.update_status(name),
            Some(UpdateStatus::UpdateAvailable { .. } | UpdateStatus::NoBaseline { .. })
        )
    }

    /// Ask GitHub whether any installed skill has moved on upstream.
    ///
    /// Batched by repository in `check_updates`, which asks
    /// `git/ref/heads/{branch}` at a few hundred bytes per repository and only
    /// walks trees when the commit moved. It still costs requests against an
    /// hourly budget, so this runs on a background task after the first scan
    /// and then only when the user asks for it — never from `render`, and
    /// never before the window has painted.
    pub(crate) fn check_for_updates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(scan) = self.scan() else {
            // Nothing to check against yet. The scan that lands next will run
            // one, because the flag is still set.
            self.check_after_scan = true;
            return;
        };
        let targets = scan.targets.clone();
        if targets.is_empty() {
            // No skill on this machine says where it came from, so there is
            // nothing to ask about and no request worth spending.
            self.updates = UpdateState::Ready(Rc::new(UpdateReport::default()));
            self.push_updates(cx);
            cx.notify();
            return;
        }

        let generation = self.generation;
        let roots = self.roots.clone();
        self.updates = UpdateState::Checking;
        cx.notify();

        self._updates_task = Some(cx.spawn_in(window, async move |this, cx| {
            let (report, cache_failure) = cx
                .background_spawn(async move {
                    let github = GitHub::from_env(UreqHttp::new());
                    let mut cache = RemoteCache::read(&roots);
                    let report = check_updates(&github, &targets, &mut cache);
                    // What this check saw is the whole point of the cache: a
                    // write that fails means the next check asks GitHub every
                    // one of these questions again.
                    let cache_failure = cache.write(&roots).map(|error| error.to_string());
                    (report, cache_failure)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                if this.generation != generation {
                    // A scan overtook the check, so this report describes a
                    // list that has changed underneath it. Nothing starts
                    // another check on its own, and leaving `Checking` set
                    // holds the Updates row on its skeleton, the list header
                    // without a count and "Update all" disabled for the rest
                    // of the session. Back to "not checked yet", which is the
                    // state that offers the check again — and cheaply, because
                    // the cache was written above. Only from `Checking`: a
                    // scan that found nothing to check has already answered.
                    if matches!(this.updates, UpdateState::Checking) {
                        this.updates = UpdateState::Idle;
                        cx.notify();
                    }
                    return;
                }
                this.adopt_updates(report, cache_failure, window, cx);
            })
            .ok();
        }));
    }

    fn adopt_updates(
        &mut self,
        report: UpdateReport,
        cache_failure: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rate_limit = report.rate_limit().or(self.rate_limit);
        let exhausted = report.hit_rate_limit();
        self.updates = UpdateState::Ready(Rc::new(report));
        self.push_updates(cx);
        cx.notify();

        if exhausted {
            window.push_notification(
                Notification::warning(self.rate_limit_sentence())
                    .title("Some skills were not checked"),
                cx,
            );
        }
        // Beside the rate-limit warning rather than instead of it: the two are
        // separate facts, and together they are the reason the limit is about
        // to be spent all over again.
        if let Some(reason) = cache_failure {
            push_notice(
                Notice::warning(
                    "Check not recorded",
                    format!(
                        "{}. What this check found could not be kept, so the next check spends \
                         GitHub's whole hourly budget asking the same questions again.",
                        capitalized(&reason)
                    ),
                ),
                window,
                cx,
            );
        }
    }

    /// Why the check stopped, and what would make it stop happening.
    ///
    /// Named as a wait rather than as a clock time: the reader wants to know
    /// how long, and "in 43 minutes" answers that without a timezone.
    pub(crate) fn rate_limit_sentence(&self) -> String {
        let mut sentence = match self.rate_limit {
            Some(limit) => format!(
                "GitHub's request limit is spent. It resets in {}.",
                in_words(limit.wait_from(std::time::SystemTime::now()).as_secs())
            ),
            None => "GitHub's request limit is spent.".to_string(),
        };
        // Only when the lookup has finished and found nothing. While it is
        // still running there is no honest way to say whether a token exists,
        // and advice about setting one would be guesswork.
        if self.token_source == Some(TokenSource::None) {
            sentence.push_str(
                " Without a token GitHub allows 60 requests an hour. Set SKILLBASE_GITHUB_TOKEN, \
                 or log in with gh, to raise that to 5000.",
            );
        }
        sentence
    }

    /// How many skills the last check found behind upstream.
    ///
    /// What "Update all" would work through, and what the Updates section
    /// counts, so the button and the sentence above it cannot disagree.
    pub(crate) fn updatable_count(&self) -> usize {
        match &self.updates {
            UpdateState::Ready(report) => report.updatable().count(),
            _ => 0,
        }
    }

    /// True while a download of any kind is running.
    ///
    /// One at a time: they share GitHub's hourly budget, and two of them
    /// writing into the store at once is not something the store is asked to
    /// survive.
    pub(crate) fn downloading(&self) -> bool {
        self.installing.is_some() || self.updating.is_some()
    }

    /// Replace every skill the last check found behind upstream.
    ///
    /// Only the copies Skillbase installed and has not seen edited since. A
    /// copy that was edited, or that carries no install record, is left where
    /// it is: replacing it deletes work the user cannot get back, and the
    /// detail pane already asks that question one skill at a time. Which ones
    /// were left, and why, goes in the summary rather than being dropped.
    pub(crate) fn update_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The same refusal a Discover install gives while this is running, in
        // the same words: one job writes into the store at a time, and the
        // strip at the foot of the window names the one that has it.
        if let Some(sentence) = self.busy_sentence("update the rest") {
            window.push_notification(Notification::info(sentence).title("Already running"), cx);
            return;
        }

        let Some((candidates, unsourced)) = self.update_candidates() else {
            return;
        };
        if candidates.is_empty() {
            window.push_notification(
                Notification::warning(format!(
                    "{} records no GitHub repository, so there is nothing to download.",
                    name_list(&unsourced, NAMED_IN_SUMMARY)
                ))
                .title("Nothing to update"),
                cx,
            );
            return;
        }

        let cancel = Arc::new(AtomicBool::new(false));
        self.updating = Some(
            Installing::new("updating", "checking which copies can be replaced")
                .cancelled_by(cancel.clone()),
        );
        self.tick_progress(window, cx);
        cx.notify();

        let roots = self.roots.clone();
        self.update_task = Some(cx.spawn_in(window, async move |this, cx| {
            // Step one: hash each directory against what was recorded at
            // install, so an edited copy is found before anything is
            // downloaded. One cache read, no network.
            let (ready, held_back) = cx
                .background_spawn({
                    let roots = roots.clone();
                    async move { sort_by_local_state(&roots, candidates) }
                })
                .await;

            // Step two: download them, one at a time. Each is a repository
            // archive, and they share the same hourly budget.
            let total = ready.len();
            let mut results: Vec<(SharedString, Result<Installed, FetchError>)> = Vec::new();
            for (index, candidate) in ready.into_iter().enumerate() {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let name = candidate.name.clone();
                let written = results.iter().filter(|(_, done)| done.is_ok()).count();
                this.update(cx, |this, cx| {
                    if let Some(progress) = &mut this.updating {
                        progress.working_on(name.clone());
                        progress.reached(index, written, total);
                    }
                    cx.notify();
                })
                .ok();

                let installed = cx
                    .background_spawn({
                        let roots = roots.clone();
                        let cancel = cancel.clone();
                        let location = candidate.location.clone();
                        // `named` keeps the directory name the skill already
                        // has, and `replacing()` is what makes this a
                        // replacement rather than a refusal — the same pair
                        // the detail pane's single-skill update uses.
                        let options = InstallOptions::new()
                            .named(candidate.name.to_string())
                            .replacing()
                            .cancelled_by(cancel);
                        async move { install_skill(&roots, &location, &options) }
                    })
                    .await;

                match installed {
                    Err(FetchError::Cancelled) => break,
                    installed => results.push((name, installed)),
                }
            }

            this.update_in(cx, |this, window, cx| {
                this.updating = None;
                cx.notify();
                let written = report_update_all(&results, &held_back, &unsourced, window, cx);
                if written > 0 {
                    // Every replaced skill has a new recorded sha, so the last
                    // report's answer for it is now wrong. Scan, then ask
                    // again.
                    this.check_after_scan = true;
                    this.rescan(None, window, cx);
                }
            })
            .ok();
        }));
    }

    /// The skills a batch update would work through, and the ones it cannot
    /// download because they name no repository.
    ///
    /// `None` when no check has landed, which is also when the control that
    /// calls this is disabled.
    fn update_candidates(&self) -> Option<(Vec<UpdateCandidate>, Vec<SharedString>)> {
        let UpdateState::Ready(report) = &self.updates else {
            return None;
        };
        let scan = self.scan()?;

        let mut candidates = Vec::new();
        let mut unsourced = Vec::new();
        for name in report.updatable() {
            let Some(skill) = scan.get(name) else {
                continue;
            };
            match skill.provenance.as_ref().and_then(github_location) {
                Some(location) => candidates.push(UpdateCandidate {
                    name: skill.name.clone(),
                    location,
                    dir: skill.origin.clone(),
                }),
                None => unsourced.push(skill.name.clone()),
            }
        }
        Some((candidates, unsourced))
    }

    /// Stop the batch update, and say what stands.
    ///
    /// Dropping the task is what makes the window answer straight away. The
    /// skills already written stay written, so the list is scanned again.
    fn cancel_update_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(progress) = self.updating.take() else {
            return;
        };
        progress.stop();
        self.update_task = None;
        cx.notify();

        let done = progress.done();
        window.push_notification(
            Notification::info(if done == 0 {
                "No skill was replaced.".to_string()
            } else {
                format!(
                    "{done} skill{} had already been replaced and {} kept.",
                    if done == 1 { "" } else { "s" },
                    if done == 1 { "is" } else { "are" }
                )
            })
            .title("Update stopped"),
            cx,
        );
        if done > 0 {
            self.check_after_scan = true;
            self.rescan(None, window, cx);
        }
    }

    /// The strip that says which skill is being replaced, and offers a way to
    /// stop it.
    ///
    /// The same strip the install shows, built by the same code, in the same
    /// place, because it answers the same question: a batch of a dozen archive
    /// downloads would otherwise be a window that sits still with nothing on it
    /// to say why.
    fn render_update_progress(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let progress = self.updating.as_ref()?;
        Some(self.render_progress("update-all", progress, Self::cancel_update_all, cx))
    }

    /// How many times a skill has been invoked, or zero when nothing recorded
    /// it. A skill with no record and a skill counted before the numbers
    /// arrive are both zero, which is why the sort menu says where the figure
    /// comes from rather than leaving the reader to guess.
    pub(crate) fn usage_count(&self, name: &str) -> u32 {
        self.usage.as_ref().map_or(0, |usage| usage.count(name))
    }

    /// Read the agents' session records and adopt the invocation counts.
    ///
    /// Only two of the fifteen agents record skill invocations at all, and
    /// reading them means walking several hundred megabytes of transcript the
    /// first time, so this runs on a background thread and the list renders
    /// without it. Subsequent reads resume from a cache and take milliseconds.
    pub(crate) fn count_usage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let roots = self.roots.clone();
        self._usage_task = Some(cx.spawn_in(window, async move |this, cx| {
            let usage = cx
                .background_spawn(async move { Usage::load(&roots) })
                .await;
            this.update(cx, |this, cx| {
                this.usage = Some(Rc::new(usage));
                cx.notify();
            })
            .ok();
        }));
    }

    /// Change the list's ordering and write the choice back.
    pub(crate) fn set_sort(
        &mut self,
        sort: SkillSort,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.preferences.sort == sort {
            return;
        }
        self.preferences.sort = sort;
        self.save_preferences(window, cx);
        cx.notify();
    }

    /// How wide the skill list opens: where the user last dragged the split, or
    /// the default when they never have.
    ///
    /// Applied as the panel's initial size rather than pushed into
    /// [`ResizableState`] once the window is up. The state's own size wins from
    /// the first layout onwards, so the preference is read once per launch and
    /// the next drag is what changes it. Calling `resize_panel` at startup
    /// would emit `ResizablePanelEvent::Resized`, and the subscription below
    /// would write the clamped result straight back over the saved width.
    fn list_width(&self) -> f32 {
        self.preferences.list_width.unwrap_or(LIST_WIDTH)
    }

    /// The scan, when one has landed.
    pub(crate) fn scan(&self) -> Option<&Rc<Scan>> {
        match &self.scan {
            ScanState::Ready(scan) => Some(scan),
            _ => None,
        }
    }

    /// Show a different scope. The selection survives if the skill is still in
    /// view; otherwise the list picks its first row.
    ///
    /// When it does not survive, the detail pane is re-pointed at another
    /// skill, which throws away an unsaved edit — so that case asks first.
    pub(crate) fn select_scope(
        &mut self,
        scope: Scope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.reselects(scope) && self.detail.read(cx).showing_dirty() {
            let this = cx.entity().downgrade();
            self.detail.update(cx, |detail, cx| {
                detail.confirm_discard(
                    "Opening another skill discards them.",
                    Box::new(move |window, cx| {
                        this.update(cx, |this, cx| this.select_scope_now(scope, window, cx))
                            .ok();
                    }),
                    window,
                    cx,
                )
            });
            return;
        }
        self.select_scope_now(scope, window, cx);
    }

    /// True when moving to `scope` would leave the selected skill out of view,
    /// so the list has to land on something else.
    fn reselects(&self, scope: Scope) -> bool {
        if self.scope == scope {
            return false;
        }
        let Some(scan) = self.scan() else {
            return false;
        };
        !self
            .selected
            .as_ref()
            .and_then(|name| scan.get(name))
            .is_some_and(|skill| self.lists(scope, skill))
    }

    fn select_scope_now(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        let was_elsewhere = self.work_area != WorkArea::Skills;
        self.work_area = WorkArea::Skills;
        if self.scope == scope {
            if was_elsewhere {
                cx.notify();
            }
            return;
        }
        self.scope = scope;
        self.save_settings(window, cx);
        // The rows underneath have changed, so a set measured against the old
        // ones no longer describes anything on screen.
        self.reset_marks();

        let Some(scan) = self.scan().cloned() else {
            cx.notify();
            return;
        };
        let still_listed = self
            .selected
            .as_ref()
            .and_then(|name| scan.get(name))
            .is_some_and(|skill| self.lists(scope, skill));
        if !still_listed {
            let first = scan
                .skills
                .iter()
                .find(|skill| self.lists(scope, skill))
                .map(|skill| skill.name.clone());
            // The newer gesture wins over whatever a scan in flight was asked
            // to land on.
            self.pending_select = None;
            self.selected = first.clone();
            self.detail
                .update(cx, |detail, cx| detail.show(scan, first, window, cx));
        }
        // Again, because the list may have landed on a different row than the
        // one that was selected when the scope changed.
        self.reset_marks();
        cx.notify();
    }

    /// Show another skill in the detail pane.
    ///
    /// The pane holds one file at a time, so this is one of the ways an unsaved
    /// edit can be lost. It asks before it takes it.
    pub(crate) fn select_skill(
        &mut self,
        name: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected.as_ref() == Some(&name) {
            return;
        }
        if self.detail.read(cx).showing_dirty() {
            let this = cx.entity().downgrade();
            self.detail.update(cx, |detail, cx| {
                detail.confirm_discard(
                    "Opening another skill discards them.",
                    Box::new(move |window, cx| {
                        this.update(cx, |this, cx| this.select_skill_now(name, window, cx))
                            .ok();
                    }),
                    window,
                    cx,
                )
            });
            return;
        }
        self.select_skill_now(name, window, cx);
    }

    fn select_skill_now(
        &mut self,
        name: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(scan) = self.scan().cloned() else {
            return;
        };
        // A scan may be in flight, carrying the skill it was asked to land on.
        // The user has just asked for a different one, and the newer gesture
        // wins.
        self.pending_select = None;
        self.selected = Some(name.clone());
        self.save_settings(window, cx);
        // A plain selection is a set of one. Whatever was marked before it was
        // measured against a gesture the user has now replaced.
        self.marks.reset_to(Some(name.clone()));
        self.detail
            .update(cx, |detail, cx| detail.show(scan, Some(name), window, cx));
        cx.notify();
    }

    /// Give the work area to one of the views that is not the skill list.
    pub(crate) fn show(&mut self, work_area: WorkArea, cx: &mut Context<Self>) {
        if self.work_area == work_area {
            return;
        }
        self.work_area = work_area;
        cx.notify();
    }

    /// Adopt a skill that has just been downloaded.
    ///
    /// The list is what shows it, so the work area comes back to the list; the
    /// scan is what proves it is there; and the check that follows is what
    /// settles whether it is already behind upstream.
    ///
    /// The scope comes back to the whole library first. A scan only keeps the
    /// skill it was asked for when the current scope lists it, and a freshly
    /// downloaded skill is in the store and visible to nobody: on an agent row,
    /// or on Unmanaged, it would be dropped and something unrelated selected
    /// while the notification said the install worked.
    pub(crate) fn installed(
        &mut self,
        name: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.work_area = WorkArea::Skills;
        self.scope = Scope::Library(Library::All);
        self.check_after_scan = true;
        // Which agents get it is the next question, and it is the one thing the
        // pane closes on every selection.
        self.open_visibility_after_scan = true;
        self.rescan(Some(name), window, cx);
    }

    /// Adopt a skill that has just been downloaded, without taking the window
    /// away from what the user was doing.
    ///
    /// Discover is a place to install several things from, and a work area that
    /// jumps to the library after each one is what stops that. The scan still
    /// runs — it is what makes the library, the sidebar counts and the Discover
    /// rows agree with the disk — it just does not move the view.
    ///
    /// Which agents can see the skill is still the next question. It is
    /// answered in the notification instead: see
    /// [`Skillbase::report_installed`].
    pub(crate) fn installed_in_place(
        &mut self,
        name: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.check_after_scan = true;
        self.rescan(Some(name), window, cx);
    }

    /// Say what an install wrote, and whether the agents on this machine can
    /// see it.
    ///
    /// One notification rather than two, because "three files landed" and "your
    /// agent cannot read them" are two halves of one answer. The button is
    /// there because the alternative is the **Visible to** switch at the foot
    /// of the detail pane, which is a long scroll away from a user who has just
    /// been told the skill is installed.
    pub(crate) fn report_installed(
        &mut self,
        title: &str,
        installed: &Installed,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let blind = cannot_see(&self.roots, &installed.name);
        let mut message = wrote_sentence(installed, &self.roots);
        if let Some(reach) = reach_sentence(&self.roots, &blind, &installed.name, false) {
            message.push(' ');
            message.push_str(&reach);
        }
        let targets = vec![(
            SharedString::from(installed.name.clone()),
            installed.dir.clone(),
        )];
        self.push_install_notification(title, message, blind, targets, window, cx);
        install_warnings(installed, window, cx);
    }

    /// The notification an install ends with.
    ///
    /// Plain success when every agent on the machine can already read the
    /// store. When some cannot, it is a [`Notice`]: it names them and carries
    /// the button that links them, and it stays until it is cleared, because a
    /// sentence about an agent that cannot see the skill is worth nothing if
    /// it goes before it can be acted on.
    ///
    /// `targets` is what the button links, as the name each skill landed under
    /// and the directory it landed in.
    pub(crate) fn push_install_notification(
        &mut self,
        title: &str,
        message: String,
        blind: Vec<&'static AgentDef>,
        targets: Vec<(SharedString, PathBuf)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if blind.is_empty() {
            window.push_notification(Notification::success(message).title(title.to_string()), cx);
            return;
        }

        let label = link_label(&blind);
        let this = cx.entity().downgrade();
        push_notice(
            Notice::info(title.to_string(), message).action(label, move |window, cx| {
                let blind = blind.clone();
                let targets = targets.clone();
                this.update(cx, |this, cx| {
                    this.link_installed(targets, blind, window, cx);
                })
                .ok();
            }),
            window,
            cx,
        );
    }

    /// Link skills that have just been installed into the agents that cannot
    /// see them.
    ///
    /// One write for the whole set rather than one per agent, so a link into
    /// four agents is one notification and one scan.
    pub(crate) fn link_installed(
        &mut self,
        targets: Vec<(SharedString, PathBuf)>,
        agents: Vec<&'static AgentDef>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if targets.is_empty() || agents.is_empty() {
            return;
        }
        let roots = self.roots.clone();

        cx.spawn_in(window, async move |this, cx| {
            let linked = cx
                .background_spawn(async move {
                    let installer = Installer::new(roots);
                    let mut done = Outcome::default();
                    for (name, origin) in &targets {
                        for agent in &agents {
                            // A refusal on the third link still leaves the
                            // first two, and the notification has to say so
                            // rather than report the refusal alone.
                            match installer.link(name, origin, agent) {
                                Ok(outcome) => done.changes.extend(outcome.changes),
                                Err(source) => return Err(InstallError::partial(done, source)),
                            }
                        }
                    }
                    Ok(done)
                })
                .await;

            this.update_in(cx, |this, window, cx| {
                report("Linked", "Could not link", linked, &this.roots, window, cx);
                // Whatever it said: the links are what the counts and the
                // agent rows are drawn from, and a refusal part-way through
                // still moved them.
                this.rescan(None, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Re-read the machine: scan every scope, count invocations again, and ask
    /// GitHub what has moved on.
    ///
    /// This is the one gesture that means "look again", so it is the one that
    /// spends requests without being asked twice.
    pub(crate) fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.check_after_scan = true;
        self.rescan(None, window, cx);
        // Cheap after the first read, which is cached.
        self.count_usage(window, cx);
    }

    /// Turn the "Show all agents" preference on or off and write it back.
    ///
    /// The sidebar changes immediately; the write is a background task, and a
    /// failure is reported rather than swallowed, because a preference that
    /// silently fails to stick is worse than one that is not offered.
    pub(crate) fn set_show_all_agents(
        &mut self,
        on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.preferences.show_all_agents == on {
            return;
        }
        self.preferences.show_all_agents = on;
        cx.notify();
        self.save_preferences(window, cx);
    }

    /// Say that the settings file was there and could not be read.
    ///
    /// Skillbase starts on the defaults either way, so this does not stop
    /// anything; it is worth saying because the next change of any setting
    /// writes the file back over whatever the user had put in it. Deferred
    /// past the first frame so the notification has a window to land in.
    fn report_settings_problem(
        &mut self,
        problem: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(problem) = problem else {
            return;
        };
        let path = Preferences::path(&self.roots);
        self._notice_task = Some(cx.spawn_in(window, async move |_, cx| {
            cx.update(|window, cx| {
                push_notice(
                    Notice::warning(
                        "Could not read the settings",
                        format!(
                            "{problem} Skillbase started on the defaults, and the next setting \
                             you change writes {} back over what is in it now.",
                            path.display()
                        ),
                    ),
                    window,
                    cx,
                );
            })
            .ok();
        }));
    }

    /// Write the preferences out once the user has stopped changing them.
    ///
    /// Dragging the window or the split fires its observer on every frame, and
    /// each write is a file rewritten. Holding the task in `_settle_task` means
    /// the next change drops the one waiting, so only the last state of a drag
    /// reaches the disk.
    fn save_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._settle_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(400))
                .await;
            this.update_in(cx, |this, window, cx| this.save_preferences(window, cx))
                .ok();
        }));
    }

    /// Write whatever is waiting, now, on this thread.
    ///
    /// Both ways out of the application drop this view, and dropping it drops
    /// the debounced task above, which cancels it. The window frame is written
    /// only through that path, so moving the window and pressing Cmd-Q inside
    /// 400ms would otherwise lose the move. One small file on the way out is
    /// cheaper than a preference that does not stick.
    fn flush_settings(&mut self) {
        if self._settle_task.take().is_none() {
            return;
        }
        self.sync_preferences();
        // Nothing is left to report a failure to: the window is going. The next
        // launch opens where the last written frame says, which is where the
        // window was a moment ago.
        let _ = self.preferences.save(&self.roots);
    }

    /// Copy the settings that have a live field of their own into the record
    /// that gets written.
    ///
    /// One place rather than an assignment beside each field. The three writers
    /// that used to skip theirs — a scan that fell back to another skill, a
    /// change of scope that re-selected, an install that reset the scope — each
    /// left the file describing a state the interface was never in.
    fn sync_preferences(&mut self) {
        self.preferences.scope = self.scope;
        self.preferences.selected = self.selected.as_ref().map(SharedString::to_string);
        self.preferences.sidebar_collapsed = self.sidebar_collapsed;
        self.preferences.agents_collapsed = self.agents_collapsed;
    }

    /// Write the preferences out on a background task.
    ///
    /// A failure is reported rather than swallowed, because a preference that
    /// silently fails to stick is worse than one that is not offered.
    fn save_preferences(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_preferences();
        let preferences = self.preferences.clone();
        let roots = self.roots.clone();
        cx.spawn_in(window, async move |_, cx| {
            let written = cx
                .background_spawn(async move { preferences.save(&roots) })
                .await;
            if let Err(error) = written {
                cx.update(|window, cx| {
                    // Stays until it is cleared. A failure the user glanced
                    // away from would otherwise leave the interface showing a
                    // preference the disk never took, with nothing left on
                    // screen to say so.
                    push_notice(
                        Notice::error("Could not save the setting", error.to_string()),
                        window,
                        cx,
                    );
                })
                .ok();
            }
        })
        .detach();
    }

    /// Put the caret in the search field.
    ///
    /// Find in a list-shaped application means the list's own filter, not the
    /// framework's in-document search, which answers only once a text control
    /// already has focus and so cannot be the way into one.
    pub(crate) fn find_skill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The field lives in the list column, which is not on screen while
        // another view has the work area.
        self.work_area = WorkArea::Skills;
        self.search.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    /// Cmd-W: close the file in front of the user, or the window.
    ///
    /// In a pane with tabs, Cmd-W is the tab's shortcut before it is the
    /// window's — the same order TextEdit and Xcode use — so a bundled file
    /// open in a tab is what closes first. With nothing but the Overview and
    /// `SKILL.md`, which are permanent, there is no tab to close and the
    /// gesture means the window.
    fn close_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Only while the pane is the thing on screen. Closing a tab the user
        // cannot see would look like Cmd-W doing nothing at all.
        if self.work_area == WorkArea::Skills {
            let closed = self
                .detail
                .update(cx, |detail, cx| detail.close_showing_file(window, cx));
            if closed {
                return;
            }
        }
        self.leave(
            "Closing the window discards them.",
            window,
            cx,
            |window, _| window.remove_window(),
        );
    }

    /// Cmd-Q. Closing the last window quits, so both ways out ask the same
    /// question first.
    fn quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.leave("Quitting discards them.", window, cx, |_, cx| cx.quit());
    }

    /// Leave the application by `exit`, once the unsaved edits are settled.
    fn leave(
        &mut self,
        loss: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        exit: impl FnOnce(&mut Window, &mut App) + 'static,
    ) {
        // Before either branch: `exit` takes the window away, and the dialog
        // branch runs it from a callback this view no longer holds a task for.
        self.flush_settings();
        if self.detail.read(cx).showing_dirty() {
            self.detail.update(cx, |detail, cx| {
                detail.confirm_discard(loss, Box::new(exit) as Proceed, window, cx)
            });
            return;
        }
        exit(window, cx);
    }

    /// Keep whether the Agents group is open, once the user has said.
    pub(crate) fn remember_agents_group(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_settings(window, cx);
    }

    pub(crate) fn toggle_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        self.save_settings(window, cx);
        cx.notify();
    }

    /// The way back to a hidden sidebar, offered by whichever column is
    /// leftmost while it is gone.
    ///
    /// The `Button` gpui-component's `SidebarToggleButton` wraps, spelled out
    /// here because that wrapper takes no name and no tooltip, and this
    /// control shows nothing but a glyph.
    pub(crate) fn sidebar_reopen(&self, cx: &mut Context<Self>) -> Option<Button> {
        self.sidebar_collapsed.then(|| {
            Button::new("collapse")
                .ghost()
                .small()
                .icon(Icon::new(IconName::PanelLeftOpen).size_4())
                .tooltip("Show sidebar")
                .accessibility_label("Show sidebar")
                .on_click(cx.listener(|this, _, window, cx| this.toggle_sidebar(window, cx)))
        })
    }

    /// The first header band of a work-area column.
    ///
    /// With the sidebar hidden this column is the leftmost one, so its band has
    /// to leave the macOS traffic lights their room. A `drag_band` does the
    /// window move and the double-click zoom; the inset is the same 80px
    /// `TitleBar` would have used, without inheriting its 34px default height.
    pub(crate) fn column_band(
        &self,
        id: &'static str,
        row: impl IntoElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.sidebar_collapsed {
            drag_band(id, window, cx)
                .flex_shrink_0()
                .h(BAND_HEIGHT)
                .pl(px(TRAFFIC_LIGHT_INSET))
                .items_center()
                .child(row)
                .into_any_element()
        } else {
            drag_band(id, window, cx)
                .flex_shrink_0()
                .h(BAND_HEIGHT)
                .child(row)
                .into_any_element()
        }
    }
}

/// Where a provenance says the skill can be downloaded from again, when it
/// names a GitHub repository that can be read.
fn github_location(provenance: &Provenance) -> Option<SkillLocation> {
    let (owner, repo) = provenance.owner_repo()?;
    Some(SkillLocation::new(
        RepoRef::new(owner, repo, &provenance.reference),
        &provenance.path,
    ))
}

/// Split the candidates into the ones a batch may replace outright and the
/// names of the ones it may not.
///
/// Pristine means the directory still holds the bytes that were installed, so
/// nothing is lost by overwriting it. Edited means the user's own work is in
/// there; unknown means Skillbase has no install record and cannot tell. Both
/// need the confirmation the detail pane already puts one skill at a time, so
/// a batch leaves them alone rather than answering it on the user's behalf.
///
/// Blocking: one cache read and one directory hash per candidate.
fn sort_by_local_state(
    roots: &Roots,
    candidates: Vec<UpdateCandidate>,
) -> (Vec<UpdateCandidate>, Vec<SharedString>) {
    let cache = RemoteCache::read(roots);
    let mut ready = Vec::new();
    let mut held_back = Vec::new();
    for candidate in candidates {
        match local_state(&cache, &candidate.name, &candidate.dir) {
            LocalState::Pristine => ready.push(candidate),
            LocalState::Edited | LocalState::Unknown => held_back.push(candidate.name.clone()),
        }
    }
    (ready, held_back)
}

/// One summary for the whole batch, rather than a notification per skill.
///
/// It counts against everything the check said was behind upstream, not
/// against what was attempted, so a run that replaced three of seven cannot
/// read as a complete success. Returns how many were written.
fn report_update_all(
    results: &[(SharedString, Result<Installed, FetchError>)],
    held_back: &[SharedString],
    unsourced: &[SharedString],
    window: &mut Window,
    cx: &mut App,
) -> usize {
    let written = results.iter().filter(|(_, done)| done.is_ok()).count();
    let failures: Vec<String> = results
        .iter()
        .filter_map(|(name, done)| done.as_ref().err().map(|error| format!("{name}: {error}")))
        .collect();
    let behind = results.len() + held_back.len() + unsourced.len();

    let mut message = if written == 0 {
        format!("None of the {behind} skills behind upstream were updated.")
    } else {
        format!("{written} of {behind} skills behind upstream were updated.")
    };
    for failure in failures.iter().take(NAMED_IN_SUMMARY) {
        message.push('\n');
        message.push_str(failure);
    }
    if failures.len() > NAMED_IN_SUMMARY {
        message.push_str(&format!(
            "\nand {} more that did not download.",
            failures.len() - NAMED_IN_SUMMARY
        ));
    }
    if !held_back.is_empty() {
        message.push_str(&format!(
            "\nLeft alone: {}. Each has been edited since it was installed, or has no install \
             record, so replacing it is a decision to take one skill at a time on its own page.",
            name_list(held_back, NAMED_IN_SUMMARY)
        ));
    }
    if !unsourced.is_empty() {
        message.push_str(&format!(
            "\nLeft alone: {} records no GitHub repository, so there is nothing to download.",
            name_list(unsourced, NAMED_IN_SUMMARY)
        ));
    }
    // Each download sweeps the staging directory on its way in. A sweep that
    // could not remove what an earlier run abandoned is the same fact however
    // many downloads reported it, so it is said once.
    let staging = results
        .iter()
        .filter_map(|(_, done)| done.as_ref().ok())
        .find_map(|installed| installed.staging.warning());
    if let Some(staging) = &staging {
        message.push('\n');
        message.push_str(staging);
    }

    let clean =
        failures.is_empty() && held_back.is_empty() && unsourced.is_empty() && staging.is_none();
    if written > 0 && clean {
        window.push_notification(Notification::success(message).title("Updated"), cx);
    } else if written == 0 {
        // Nothing changed on screen, so this sentence is the only thing that
        // says why. It stays until it is cleared.
        push_notice(Notice::warning("Nothing was updated", message), window, cx);
    } else {
        push_notice(
            Notice::warning(format!("Updated {written} of {behind}"), message),
            window,
            cx,
        );
    }
    // Every skill here was installed through `install_skill`, so a failed
    // install record is reported the same way a single install reports it.
    if let Some(reason) = take_cache_failure() {
        push_notice(
            cache_failure_notification("What was downloaded could not be recorded", &reason),
            window,
            cx,
        );
    }
    written
}

/// Names, in prose, with the tail counted rather than listed.
fn name_list(names: &[SharedString], cap: usize) -> String {
    let shown: Vec<&str> = names.iter().take(cap).map(SharedString::as_ref).collect();
    let rest = names.len() - shown.len();
    if rest == 0 {
        shown.join(", ")
    } else {
        format!("{} and {rest} more", shown.join(", "))
    }
}

/// What a fresh token lookup found, said in the terms the user can act on.
fn token_notification(source: TokenSource) -> Notification {
    match source {
        TokenSource::Environment => Notification::success(format!(
            "{GITHUB_TOKEN_ENV} is set. GitHub allows 5000 requests an hour."
        ))
        .title("Token found"),
        TokenSource::GitHubCli => Notification::success(
            "Using the token from GitHub CLI. GitHub allows 5000 requests an hour, and the token \
             stays in memory.",
        )
        .title("Token found"),
        TokenSource::None => Notification::warning(format!(
            "GitHub allows 60 requests an hour without one. Set {GITHUB_TOKEN_ENV}, or run gh \
             auth login, then look again."
        ))
        .title("No token found"),
    }
}

impl Render for Skillbase {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .text_sm()
            .track_focus(&self.focus)
            // The menu bar's own commands. They hang off the outermost element
            // so that they are reachable whatever has focus, including from
            // inside a dialog, and each one does exactly what the control that
            // already offers it does.
            .on_action(
                cx.listener(|this, _: &NewSkill, window, cx| {
                    this.open_new_skill_dialog(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ShowSettings, _, cx| this.show(WorkArea::Settings, cx)),
            )
            .on_action(cx.listener(|this, _: &ReloadSkills, window, cx| this.refresh(window, cx)))
            .on_action(
                cx.listener(|this, _: &ToggleSidebar, window, cx| this.toggle_sidebar(window, cx)),
            )
            .on_action(cx.listener(|this, _: &FindSkill, window, cx| this.find_skill(window, cx)))
            // Exactly what the detail pane's Save button commits, so Cmd-S and
            // the button cannot mean two different things. The pane decides
            // which tab is being written; this only asks.
            .on_action(cx.listener(|this, _: &Save, window, cx| {
                this.detail
                    .update(cx, |detail, cx| detail.save_active(window, cx));
            }))
            // The rest of what the detail pane's buttons do. Each one forwards
            // into the method the button already calls, and each is a no-op
            // when there is no skill to act on — the pane's own guards — so the
            // menu never claims to have done something it did not.
            // Delete acts on whatever is marked. With one row marked that is
            // the selected skill and the pane's own confirmation, which is what
            // the command has always meant; with several it is one confirmation
            // for the set.
            .on_action(cx.listener(|this, _: &DeleteSkill, window, cx| {
                if this.marks.len() > 1 {
                    this.confirm_delete_marked(window, cx);
                    return;
                }
                this.detail
                    .update(cx, |detail, cx| detail.confirm_delete(window, cx));
            }))
            // The other two bulk commands. Each is a no-op below two marked
            // rows, so the menu never claims to have done something it did not.
            .on_action(cx.listener(|this, _: &LinkMarked, window, cx| {
                this.open_link_marked_dialog(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &UnlinkMarked, window, cx| {
                this.open_link_marked_dialog(false, window, cx)
            }))
            // The marking commands are registered here as well as on the list.
            // Their keys are bound to the list's own context, but a menu item
            // is dispatched along whatever holds focus, and the list element is
            // not on that path when focus is in the editor, the search field or
            // Discover. Both routes end in the same two methods.
            .on_action(cx.listener(|this, _: &MarkAll, _, cx| this.mark_all(cx)))
            .on_action(cx.listener(|this, _: &ClearMarks, _, cx| this.clear_marks(cx)))
            .on_action(cx.listener(|this, _: &RevealInFinder, window, cx| {
                this.detail
                    .update(cx, |detail, cx| detail.reveal(window, cx));
            }))
            .on_action(cx.listener(|this, _: &UpdateSkill, window, cx| {
                this.detail
                    .update(cx, |detail, cx| detail.update_skill(window, cx));
            }))
            .on_action(
                cx.listener(|this, _: &ShowDiscover, window, cx| this.show_discover(window, cx)),
            )
            .on_action(cx.listener(|this, _: &InstallFromGitHub, window, cx| {
                this.open_install_dialog(window, cx)
            }))
            .on_action(cx.listener(|this, _: &CheckForUpdates, window, cx| {
                this.check_for_updates(window, cx)
            }))
            // Both of these have a global handler in `menus`, which is what
            // answers when no window is on the dispatch path. A view handler
            // runs first and stops there, so an unsaved edit is settled before
            // either one takes the window away.
            .on_action(
                cx.listener(|this, _: &CloseWindow, window, cx| this.close_window(window, cx)),
            )
            .on_action(cx.listener(|this, _: &Quit, window, cx| this.quit(window, cx)))
            .child(
                // `h_flex` centres its children, so every pane in this row asks
                // for the full height explicitly.
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    // Collapsed, the sidebar column goes away rather than
                    // shrinking to a rail: the traffic lights alone are wider
                    // than the rail would be, and they would land on the list.
                    .when(!self.sidebar_collapsed, |this| {
                        this.child(self.render_sidebar(window, cx))
                    })
                    // Settings and Discover each replace the list and the
                    // detail pane together. Neither is a property of the
                    // selected skill: one is a view of the machine, the other
                    // of a registry the machine has nothing from yet.
                    .child(match self.work_area {
                        WorkArea::Settings => div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(self.render_settings(window, cx)),
                        WorkArea::Discover => div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(self.render_discover(window, cx)),
                        WorkArea::Skills => div().flex().flex_1().min_w_0().h_full().child(
                            h_resizable("panes")
                                .with_state(&self.panes)
                                .child(
                                    resizable_panel()
                                        .h_full()
                                        .size(px(self.list_width()))
                                        .size_range(px(LIST_MIN_WIDTH)..px(LIST_MAX_WIDTH))
                                        .child(self.render_skill_list(window, cx)),
                                )
                                .child(
                                    resizable_panel()
                                        .h_full()
                                        .size_range(px(DETAIL_MIN_WIDTH)..px(f32::MAX))
                                        .child(self.detail.clone()),
                                ),
                        ),
                    }),
            )
            // A download can be started from Discover or from the menu bar, so
            // the strip that names it sits under the columns rather than in one
            // of them: whichever view is on screen, it is the same strip and
            // the same Cancel.
            .children(self.render_install_progress(cx))
            .children(self.render_update_progress(cx))
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_sheet_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_list_of_names_is_spelled_out_in_full() {
        let names = [SharedString::from("alpha"), SharedString::from("beta")];
        assert_eq!(name_list(&names, 3), "alpha, beta");
    }

    /// A batch can hold thirty skills, and a notification that listed all of
    /// them would be a wall of text nobody reads to the end of.
    #[test]
    fn a_long_list_names_a_few_and_counts_the_rest() {
        let names: Vec<SharedString> = ["alpha", "beta", "gamma", "delta", "epsilon"]
            .iter()
            .map(|name| SharedString::from(*name))
            .collect();
        assert_eq!(name_list(&names, 3), "alpha, beta, gamma and 2 more");
    }

    #[test]
    fn a_provenance_without_a_github_repository_has_nowhere_to_download_from() {
        let mut provenance = Provenance::new(
            "https://example.com/not-github",
            "main",
            "abc123",
            "skills/x",
        );
        assert!(github_location(&provenance).is_none());

        provenance = Provenance::new(
            "https://github.com/owner/repo",
            "main",
            "abc123",
            "skills/x",
        );
        let location = github_location(&provenance).expect("a GitHub repository");
        assert_eq!(location.repo.slug(), "owner/repo");
        assert_eq!(location.path, "skills/x");
    }
}
