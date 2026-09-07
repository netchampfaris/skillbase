//! The application view: three columns, each opening with its own header band.
//!
//! This view owns the one [`Roots`] every read and every write resolves
//! against, the scan that comes back from it, the selection and the search
//! field. The detail pane owns editing and the mutations that follow from it,
//! and asks for a re-scan when it has changed the disk.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Root, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription, Task, Window,
    div, px,
};
use skillbase_core::{
    GitHub, RateLimit, RemoteCache, Roots, TokenSource, UpdateReport, UpdateStatus, UreqHttp,
    Usage, check_updates, github_token_source,
};

use crate::menus::{FindSkill, NewSkill, ReloadSkills, Save, ShowSettings, ToggleSidebar};
use crate::ui::detail::{DetailEvent, DetailPane};
use crate::ui::discover::SearchState;
use crate::ui::list::{LIST_MAX_WIDTH, LIST_MIN_WIDTH, LIST_WIDTH};
use crate::ui::model::{Library, Preferences, Scan, Scope, SkillSort, in_words, resolve_roots};
use crate::ui::{BAND_HEIGHT, DETAIL_MIN_WIDTH, TRAFFIC_LIGHT_INSET, drag_band};

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
    pub(crate) sidebar_collapsed: bool,
    /// Whether the Agents group in the sidebar is closed. Closed on launch:
    /// the Library rows are the usual destination, and a dozen agent names
    /// under them is a list the user opens when they need it.
    pub(crate) agents_collapsed: bool,
    /// Which view has the work area.
    pub(crate) work_area: WorkArea,
    /// What the user chose last time. Read once at startup and written back on
    /// every change.
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
    /// True while a download is in flight, so a second click cannot start a
    /// second one over the top of it.
    pub(crate) installing: bool,
    /// The Discover pane's search field.
    pub(crate) discover_query: Entity<InputState>,
    /// What skills.sh last said, or why it did not say anything.
    pub(crate) discover: SearchState,
    /// Bumped on every keystroke, so a search that lands after a newer one was
    /// typed is dropped rather than shown.
    pub(crate) discover_generation: u64,
    /// Where the GitHub token came from at startup. Read once, because neither
    /// the environment nor a `gh` login is watched for changes.
    pub(crate) token_source: TokenSource,
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
    _scan_task: Option<Task<()>>,
    _usage_task: Option<Task<()>>,
    _updates_task: Option<Task<()>>,
    pub(crate) _discover_task: Option<Task<()>>,
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
        let preferences = Preferences::load(&roots);

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
            // Typing in the search field changes what the list shows.
            cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
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
        ];

        let mut this = Self {
            roots,
            home_overridden,
            scan: ScanState::Loading,
            scope: Scope::Library(Library::All),
            selected: None,
            sidebar_collapsed: false,
            agents_collapsed: true,
            work_area: WorkArea::Skills,
            preferences,
            search,
            usage: None,
            new_name,
            new_description,
            install_spec,
            installing: false,
            discover_query,
            discover: SearchState::Idle,
            discover_generation: 0,
            token_source: github_token_source(),
            updates: UpdateState::Idle,
            rate_limit: None,
            check_after_scan: true,
            detail,
            panes,
            focus,
            generation: 0,
            _scan_task: None,
            _usage_task: None,
            _updates_task: None,
            _discover_task: None,
            _subscriptions: subscriptions,
        };
        match resolved {
            Ok(_) => {
                this.rescan(None, window, cx);
                this.count_usage(window, cx);
            }
            Err(error) => this.scan = ScanState::Failed(error.to_string().into()),
        }
        this
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
                this.apply_scan(Rc::new(scan), select, window, cx);
            })
            .ok();
        }));
    }

    fn apply_scan(
        &mut self,
        scan: Rc<Scan>,
        select: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Keep the selection when the skill survived the change; otherwise fall
        // back to the first skill the current scope lists.
        let wanted = select.or_else(|| self.selected.clone());
        let selected = wanted
            .filter(|name| scan.get(name).is_some_and(|skill| self.scope.shows(skill)))
            .or_else(|| {
                scan.skills
                    .iter()
                    .find(|skill| self.scope.shows(skill))
                    .map(|skill| skill.name.clone())
            });

        self.selected = selected.clone();
        self.scan = ScanState::Ready(scan.clone());
        self.detail
            .update(cx, |detail, cx| detail.show(scan, selected, window, cx));
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
            let report = cx
                .background_spawn(async move {
                    let github = GitHub::from_env(UreqHttp::new());
                    let mut cache = RemoteCache::read(&roots);
                    let report = check_updates(&github, &targets, &mut cache);
                    cache.write(&roots);
                    report
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                if this.generation != generation {
                    return;
                }
                this.adopt_updates(report, window, cx);
            })
            .ok();
        }));
    }

    fn adopt_updates(&mut self, report: UpdateReport, window: &mut Window, cx: &mut Context<Self>) {
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
        if !self.token_source.has_token() {
            sentence.push_str(
                " Without a token GitHub allows 60 requests an hour. Set SKILLBASE_GITHUB_TOKEN, \
                 or log in with gh, to raise that to 5000.",
            );
        }
        sentence
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

    /// The scan, when one has landed.
    pub(crate) fn scan(&self) -> Option<&Rc<Scan>> {
        match &self.scan {
            ScanState::Ready(scan) => Some(scan),
            _ => None,
        }
    }

    /// Show a different scope. The selection survives if the skill is still in
    /// view; otherwise the list picks its first row.
    pub(crate) fn select_scope(
        &mut self,
        scope: Scope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_elsewhere = self.work_area != WorkArea::Skills;
        self.work_area = WorkArea::Skills;
        if self.scope == scope {
            if was_elsewhere {
                cx.notify();
            }
            return;
        }
        self.scope = scope;

        let Some(scan) = self.scan().cloned() else {
            cx.notify();
            return;
        };
        let still_listed = self
            .selected
            .as_ref()
            .and_then(|name| scan.get(name))
            .is_some_and(|skill| scope.shows(skill));
        if !still_listed {
            let first = scan
                .skills
                .iter()
                .find(|skill| scope.shows(skill))
                .map(|skill| skill.name.clone());
            self.selected = first.clone();
            self.detail
                .update(cx, |detail, cx| detail.show(scan, first, window, cx));
        }
        cx.notify();
    }

    pub(crate) fn select_skill(
        &mut self,
        name: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected.as_ref() == Some(&name) {
            return;
        }
        let Some(scan) = self.scan().cloned() else {
            return;
        };
        self.selected = Some(name.clone());
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
    pub(crate) fn installed(
        &mut self,
        name: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.work_area = WorkArea::Skills;
        self.check_after_scan = true;
        self.rescan(Some(name), window, cx);
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

    /// Write the preferences out on a background task.
    ///
    /// A failure is reported rather than swallowed, because a preference that
    /// silently fails to stick is worse than one that is not offered.
    fn save_preferences(&self, window: &mut Window, cx: &mut Context<Self>) {
        let preferences = self.preferences;
        let roots = self.roots.clone();
        cx.spawn_in(window, async move |_, cx| {
            let written = cx
                .background_spawn(async move { preferences.save(&roots) })
                .await;
            if let Err(error) = written {
                cx.update(|window, cx| {
                    window.push_notification(
                        // Stays until it is dismissed. A failure the user
                        // glanced away from would otherwise leave the
                        // interface showing a preference the disk never
                        // took, with nothing left on screen to say so.
                        Notification::error(error.to_string())
                            .title("Could not save the setting")
                            .autohide(false),
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

    pub(crate) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
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
                .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)))
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
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|this, _: &FindSkill, window, cx| this.find_skill(window, cx)))
            // Exactly what the detail pane's Save button commits, so Cmd-S and
            // the button cannot mean two different things. The pane decides
            // which tab is being written; this only asks.
            .on_action(cx.listener(|this, _: &Save, window, cx| {
                this.detail
                    .update(cx, |detail, cx| detail.save_active(window, cx));
            }))
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
                                        .size(px(LIST_WIDTH))
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
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_sheet_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}
