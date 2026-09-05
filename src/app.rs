//! The application view: the title bar and the three panes beneath it.

use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_kit::component::sidebar::SidebarToggleButton;
use gpui_kit::component::{ActiveTheme as _, Root, TitleBar, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Window, div, px,
};

use crate::ui::detail::DetailPane;
use crate::ui::list::{LIST_MAX_WIDTH, LIST_MIN_WIDTH, LIST_WIDTH};
use crate::ui::model::{Library, Scope, Skill, placeholder_skills};

/// The root view. It owns the selection, the search field, and the panes'
/// shared geometry; the detail pane owns its own editing state.
pub struct Skillbase {
    pub(crate) skills: Vec<Skill>,
    pub(crate) scope: Scope,
    pub(crate) selected: Option<SharedString>,
    pub(crate) sidebar_collapsed: bool,
    pub(crate) search: Entity<InputState>,
    detail: Entity<DetailPane>,
    panes: Entity<ResizableState>,
    _subscriptions: Vec<Subscription>,
}

impl Skillbase {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let skills = placeholder_skills();
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search skills"));
        let detail = cx.new(|cx| DetailPane::new(window, cx));
        let panes = cx.new(|_| ResizableState::default());

        // Typing in the search field changes what the list shows, so the
        // application view has to re-render on every change.
        let subscriptions = vec![cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        })];

        let mut this = Self {
            skills,
            scope: Scope::Library(Library::All),
            selected: None,
            sidebar_collapsed: false,
            search,
            detail,
            panes,
            _subscriptions: subscriptions,
        };

        if let Some(first) = this.skills.first().map(|skill| skill.name.clone()) {
            this.select_skill(first, window, cx);
        }
        this
    }

    /// Show a different scope. The selection survives if the skill is still
    /// in view; otherwise the pane falls back to its empty state.
    pub(crate) fn select_scope(
        &mut self,
        scope: Scope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.scope == scope {
            return;
        }
        self.scope = scope;

        let still_listed = self
            .selected
            .as_ref()
            .and_then(|name| self.skill(name))
            .is_some_and(|skill| scope.shows(skill));
        if !still_listed {
            self.selected = None;
            self.detail
                .update(cx, |detail, cx| detail.show(None, window, cx));
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
        let skill = self.skill(&name).cloned();
        self.selected = skill.as_ref().map(|skill| skill.name.clone());
        self.detail
            .update(cx, |detail, cx| detail.show(skill.as_ref(), window, cx));
        cx.notify();
    }

    fn skill(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        cx.notify();
    }

    /// The title bar carries the sidebar's colour and no bottom hairline, so
    /// it and the sidebar read as one surface running under the traffic
    /// lights.
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                            .child(self.scope.title()),
                    ),
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
                // `h_flex` centres its children, so every pane in this row
                // asks for the full height explicitly.
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(self.render_sidebar(window, cx))
                    .child(
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
                        ),
                    ),
            )
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_sheet_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}
