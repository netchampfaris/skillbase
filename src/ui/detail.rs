//! The detail pane: the skill's name and description as form fields, the
//! agents it is visible to, and the whole `SKILL.md` in a code editor.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::TextareaState;
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState, Textarea};
use gpui_kit::component::label::Label;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, ElementId, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, relative, rems,
};

use super::model::{AGENTS, Skill};

/// The right-hand pane. Owns the editing state for whichever skill is
/// selected; the application view tells it what that is.
pub struct DetailPane {
    skill: Option<Skill>,
    name: Entity<InputState>,
    description: Entity<TextareaState>,
    body: Entity<EditorState>,
    shared: bool,
    /// Ids of the agents whose switches are on.
    agents: Vec<&'static str>,
    dirty: bool,
    _subscriptions: Vec<Subscription>,
}

impl DetailPane {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
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
            skill: None,
            name,
            description,
            body,
            shared: false,
            agents: Vec::new(),
            dirty: false,
            _subscriptions: subscriptions,
        }
    }

    /// Load a skill, or clear the pane when nothing is selected.
    ///
    /// `InputState::set_value` does not emit a change event, so loading never
    /// marks the pane dirty.
    pub fn show(&mut self, skill: Option<&Skill>, window: &mut Window, cx: &mut Context<Self>) {
        let (name, description, body) = match skill {
            Some(skill) => (
                skill.name.clone(),
                skill.description.clone(),
                skill.body.clone(),
            ),
            None => (
                SharedString::default(),
                SharedString::default(),
                SharedString::default(),
            ),
        };

        self.name
            .update(cx, |state, cx| state.set_value(name, window, cx));
        self.description
            .update(cx, |state, cx| state.set_value(description, window, cx));
        self.body
            .update(cx, |state, cx| state.set_value(body, window, cx));

        self.shared = skill.is_some_and(|skill| skill.shared);
        self.agents = skill.map(|skill| skill.agents.clone()).unwrap_or_default();
        self.skill = skill.cloned();
        self.dirty = false;
        cx.notify();
    }

    fn mark_edited(&mut self, event: &InputEvent, cx: &mut Context<Self>) {
        if matches!(event, InputEvent::Change) && !self.dirty {
            self.dirty = true;
            cx.notify();
        }
    }

    fn set_shared(&mut self, shared: bool, cx: &mut Context<Self>) {
        self.shared = shared;
        self.dirty = true;
        cx.notify();
    }

    fn set_agent(&mut self, id: &'static str, on: bool, cx: &mut Context<Self>) {
        self.agents.retain(|agent| *agent != id);
        if on {
            self.agents.push(id);
        }
        self.dirty = true;
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        // Writing back to disk belongs to `skillbase-core`; the shell only
        // owns the state that says something is pending.
        self.dirty = false;
        cx.notify();
    }

    fn empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
    }

    fn header(&self, skill: &Skill, cx: &mut Context<Self>) -> impl IntoElement {
        let dirty = self.dirty;

        h_flex()
            .flex_shrink_0()
            .h_12()
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
                    .when(!skill.valid, |this| {
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
                Button::new("save")
                    .primary()
                    .small()
                    .label("Save")
                    .disabled(!dirty)
                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
            )
    }

    fn visibility(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let shared = self.shared;

        v_flex()
            .flex_shrink_0()
            .gap_3()
            .child(section_title("Visible to", cx))
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Switch::new("visible-shared")
                            .checked(shared)
                            .label("Shared")
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.set_shared(*checked, cx)
                            })),
                    )
                    .child(
                        div()
                            .pl_10()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("Links into ~/.agents/skills, which every agent below reads except Claude Code."),
                    ),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_x_8()
                    .gap_y_2()
                    .children(AGENTS.iter().map(|agent| {
                        let on = self.agents.contains(&agent.id);
                        div().w(relative(0.44)).child(
                            Switch::new((ElementId::from("visible"), agent.id))
                                .checked(on)
                                .label(agent.label)
                                .tooltip(agent.effect)
                                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                    this.set_agent(agent.id, *checked, cx)
                                })),
                        )
                    })),
            )
    }
}

fn section_title(label: &'static str, cx: &mut Context<DetailPane>) -> impl IntoElement {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(label)
}

impl Render for DetailPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(skill) = self.skill.clone() else {
            return self.empty_state(cx).into_any_element();
        };

        v_flex()
            .size_full()
            .min_w_0()
            .bg(cx.theme().background)
            .child(self.header(&skill, cx))
            .child(
                v_flex()
                    // One scroll owner for the pane; the editor keeps its own
                    // for the file it holds.
                    .id("detail-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_5()
                    .pb_5()
                    .gap_6()
                    .child(
                        v_flex()
                            .flex_shrink_0()
                            .gap_4()
                            .child(
                                v_flex()
                                    .gap_2()
                                    .child(Label::new("Name"))
                                    .child(Input::new(&self.name).small()),
                            )
                            .child(
                                v_flex()
                                    .gap_2()
                                    .child(Label::new("Description"))
                                    .child(Textarea::new(&self.description).h(rems(4.5))),
                            ),
                    )
                    .child(self.visibility(cx))
                    .child(
                        v_flex()
                            .flex_shrink_0()
                            .gap_2()
                            .child(section_title("SKILL.md", cx))
                            // A definite height: the editor scrolls the file
                            // rather than growing the pane to fit it.
                            .child(Editor::new(&self.body).h(rems(20.))),
                    )
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .gap_2()
                            .items_center()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(Icon::new(IconName::Folder).xsmall())
                            .child(div().truncate().child(skill.origin.clone())),
                    ),
            )
            .into_any_element()
    }
}
