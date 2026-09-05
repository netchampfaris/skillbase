//! The application view: the title bar and the three panes beneath it.
//!
//! This view owns the one [`Roots`] every read and every write resolves
//! against, the scan that comes back from it, the selection and the search
//! field. The detail pane owns editing and the mutations that follow from it,
//! and asks for a re-scan when it has changed the disk.

use std::rc::Rc;

use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_kit::component::sidebar::SidebarToggleButton;
use gpui_kit::component::{ActiveTheme as _, Root, TitleBar, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Task, Window, div, px,
};
use skillbase_core::Roots;

use crate::ui::detail::{DetailEvent, DetailPane};
use crate::ui::list::{LIST_MAX_WIDTH, LIST_MIN_WIDTH, LIST_WIDTH};
use crate::ui::model::{Library, Preferences, Scan, Scope, resolve_roots};

/// Where the scan has got to. A scan of every scope on a busy machine takes
/// long enough that the first paint must not wait for it.
pub(crate) enum ScanState {
    Loading,
    Ready(Rc<Scan>),
    Failed(SharedString),
}

/// The root view.
pub struct Skillbase {
    /// Every path the application reads or writes hangs off this one value.
    pub(crate) roots: Roots,
    /// True when `SKILLBASE_HOME` pointed the application somewhere other than
    /// the real home. Said out loud in the title bar, because every mutation
    /// lands there.
    pub(crate) home_overridden: bool,
    pub(crate) scan: ScanState,
    pub(crate) scope: Scope,
    pub(crate) selected: Option<SharedString>,
    pub(crate) sidebar_collapsed: bool,
    /// True while the Settings pane has the work area. The scope and the
    /// selection are left as they were, so closing Settings comes back to the
    /// same skill.
    pub(crate) showing_settings: bool,
    /// What the user chose last time. Read once at startup and written back on
    /// every change.
    pub(crate) preferences: Preferences,
    pub(crate) search: Entity<InputState>,
    /// The New skill dialog's two fields. Held here rather than rebuilt each
    /// time the dialog opens, because the dialog's content builder is called on
    /// every frame it is on screen.
    pub(crate) new_name: Entity<InputState>,
    pub(crate) new_description: Entity<TextareaState>,
    detail: Entity<DetailPane>,
    panes: Entity<ResizableState>,
    /// Bumped on every scan so a result that arrives after a newer request is
    /// dropped rather than applied.
    generation: u64,
    _scan_task: Option<Task<()>>,
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
        let detail = cx.new(|cx| DetailPane::new(roots.clone(), window, cx));
        let panes = cx.new(|_| ResizableState::default());

        let subscriptions = vec![
            // Typing in the search field changes what the list shows.
            cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&detail, window, Self::on_detail_event),
        ];

        let mut this = Self {
            roots,
            home_overridden,
            scan: ScanState::Loading,
            scope: Scope::Library(Library::All),
            selected: None,
            sidebar_collapsed: false,
            showing_settings: false,
            preferences,
            search,
            new_name,
            new_description,
            detail,
            panes,
            generation: 0,
            _scan_task: None,
            _subscriptions: subscriptions,
        };
        match resolved {
            Ok(_) => this.rescan(None, window, cx),
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
        cx.notify();
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
        }
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
        let was_showing_settings = std::mem::take(&mut self.showing_settings);
        if self.scope == scope {
            if was_showing_settings {
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

    /// Give the work area to Settings.
    pub(crate) fn show_settings(&mut self, cx: &mut Context<Self>) {
        if self.showing_settings {
            return;
        }
        self.showing_settings = true;
        cx.notify();
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

        let preferences = self.preferences;
        let roots = self.roots.clone();
        cx.spawn_in(window, async move |_, cx| {
            let written = cx
                .background_spawn(async move { preferences.save(&roots) })
                .await;
            if let Err(error) = written {
                cx.update(|window, cx| {
                    window.push_notification(
                        Notification::error(error.to_string()).title("Could not save the setting"),
                        cx,
                    );
                })
                .ok();
            }
        })
        .detach();
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        cx.notify();
    }

    /// The title bar carries the sidebar's colour and no bottom hairline, so it
    /// and the sidebar read as one surface running under the traffic lights.
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let home = self.roots.home().display().to_string();

        TitleBar::new()
            .bg(cx.theme().title_bar)
            .border_color(cx.theme().title_bar)
            .child(
                h_flex()
                    .h_full()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .child(
                        SidebarToggleButton::new()
                            .collapsed(self.sidebar_collapsed)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx))),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(if self.showing_settings {
                                "Settings"
                            } else {
                                self.scope.title()
                            }),
                    )
                    .when(self.home_overridden, |this| {
                        // Every mutation lands under this directory, so it is
                        // named rather than implied.
                        this.child(
                            div()
                                .px_2()
                                .rounded(cx.theme().radius)
                                .bg(cx.theme().warning.opacity(0.15))
                                .text_xs()
                                .text_color(cx.theme().warning)
                                .child(format!("SKILLBASE_HOME={home}")),
                        )
                    }),
            )
    }
}

impl Render for Skillbase {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .text_sm()
            .child(self.render_title_bar(cx))
            .child(
                // `h_flex` centres its children, so every pane in this row asks
                // for the full height explicitly.
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(self.render_sidebar(window, cx))
                    .child(if self.showing_settings {
                        // Settings replaces the list and the detail pane
                        // together: it is a whole view of the machine, not a
                        // property of the selected skill.
                        div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(self.render_settings(cx))
                    } else {
                        div().flex().flex_1().min_w_0().h_full().child(
                            h_resizable("panes")
                                .with_state(&self.panes)
                                .child(
                                    resizable_panel()
                                        .h_full()
                                        .size(px(LIST_WIDTH))
                                        .size_range(px(LIST_MIN_WIDTH)..px(LIST_MAX_WIDTH))
                                        .child(self.render_skill_list(window, cx)),
                                )
                                .child(resizable_panel().h_full().child(self.detail.clone())),
                        )
                    }),
            )
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_sheet_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}
