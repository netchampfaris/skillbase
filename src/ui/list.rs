//! The middle pane: a search field over a scrolling list of skill rows.

use gpui_kit::component::input::Input;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, ElementId, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div,
};

use crate::app::Skillbase;

use super::model::{Skill, filter};

/// A comfortable default: long enough for a skill name plus a description
/// fragment, short enough that the detail pane keeps the surplus.
pub const LIST_WIDTH: f32 = 320.;
pub const LIST_MIN_WIDTH: f32 = 240.;
pub const LIST_MAX_WIDTH: f32 = 460.;

/// How many agent names a row shows before it starts counting.
const BADGES: usize = 3;

impl Skillbase {
    pub(crate) fn render_skill_list(
        &self,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let query = self.search.read(cx).value();
        let matches = filter(&self.skills, self.scope, &query);
        let is_empty = matches.is_empty();

        let mut rows = Vec::with_capacity(matches.len());
        for skill in matches {
            let selected = self.selected.as_ref() == Some(&skill.name);
            rows.push(self.render_skill_row(skill, selected, cx));
        }

        v_flex()
            .size_full()
            .min_w_0()
            .bg(cx.theme().background)
            .child(
                h_flex().flex_shrink_0().h_12().px_3().items_center().child(
                    Input::new(&self.search)
                        .small()
                        .cleanable(true)
                        .prefix(Icon::new(IconName::Search).small()),
                ),
            )
            .child(
                div()
                    .id("skill-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_2()
                    .pb_2()
                    .when(is_empty, |this| {
                        this.child(
                            v_flex()
                                .py_8()
                                .px_2()
                                .gap_1()
                                .child(div().text_sm().child("Nothing here"))
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(if query.is_empty() {
                                            "No skills in this scope."
                                        } else {
                                            "No skill matches that search."
                                        }),
                                ),
                        )
                    })
                    .children(rows),
            )
    }

    fn render_skill_row(
        &self,
        skill: &Skill,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = skill.name.clone();
        let agents = skill.visible_to();
        let description = skill.description.clone();
        let has_description = !description.is_empty();

        v_flex()
            // Identity comes from the skill's name, so a row keeps its state
            // when the list is filtered or reordered.
            .id(ElementId::from((
                ElementId::from("skill-row"),
                name.clone(),
            )))
            .w_full()
            .px_3()
            .py_2()
            .gap_1()
            .rounded(cx.theme().radius)
            .when(selected, |this| this.bg(cx.theme().list_active))
            .when(!selected, |this| {
                this.hover(|this| this.bg(cx.theme().list_hover))
            })
            .on_click(
                cx.listener(move |this, _, window, cx| this.select_skill(name.clone(), window, cx)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .min_w_0()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .font_medium()
                            .truncate()
                            .child(skill.name.clone()),
                    )
                    .when(!skill.valid, |this| {
                        this.child(
                            Icon::new(IconName::TriangleAlert)
                                .xsmall()
                                .text_color(cx.theme().warning),
                        )
                    }),
            )
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .truncate()
                    .child(if has_description {
                        description
                    } else {
                        "No description — the frontmatter is missing it.".into()
                    }),
            )
            .when(!agents.is_empty(), |this| {
                // The row is a scanning surface, not a full report: name the
                // first few agents and count the rest.
                let overflow = agents.len().saturating_sub(BADGES);
                this.child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_1()
                        .overflow_hidden()
                        .children(
                            agents
                                .into_iter()
                                .take(BADGES)
                                .map(|label| Tag::secondary().xsmall().child(label)),
                        )
                        .when(overflow > 0, |this| {
                            this.child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("+{overflow}")),
                            )
                        }),
                )
            })
            .into_any_element()
    }
}
