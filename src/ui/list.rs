//! The middle pane: a search field over a scrolling list of skill rows, with
//! New and Refresh in the header.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::label::Label;
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, ElementId, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    px, rems,
};
use skillbase_core::Installer;

use crate::app::{ScanState, Skillbase};

use super::model::{SkillView, agent_label};
use super::report;

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
        let warnings = self
            .scan()
            .map(|scan| scan.warnings.clone())
            .filter(|warnings| !warnings.is_empty());

        let body = match &self.scan {
            ScanState::Loading => v_flex()
                .px_3()
                .py_2()
                .gap_4()
                .children((0..6).map(|row| {
                    v_flex()
                        .id(ElementId::from(("skeleton", row as usize)))
                        .gap_2()
                        .child(Skeleton::new().h(rems(0.9)).w(rems(9.)))
                        .child(Skeleton::new().h(rems(0.8)).w_full())
                }))
                .into_any_element(),
            ScanState::Failed(error) => empty_state("Could not scan", error.clone(), cx),
            ScanState::Ready(scan) => {
                let matches = scan.filter(self.scope, &query);
                if matches.is_empty() {
                    empty_state(
                        "Nothing here",
                        if query.is_empty() {
                            "No skills in this scope.".into()
                        } else {
                            "No skill matches that search.".into()
                        },
                        cx,
                    )
                } else {
                    let rows: Vec<_> = matches
                        .into_iter()
                        .map(|skill| {
                            let selected = self.selected.as_ref() == Some(&skill.name);
                            self.render_skill_row(skill, selected, cx)
                        })
                        .collect();
                    v_flex().children(rows).into_any_element()
                }
            }
        };

        v_flex()
            .size_full()
            .min_w_0()
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .flex_shrink_0()
                    .h_12()
                    .px_3()
                    .gap_1()
                    .items_center()
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.search)
                                .small()
                                .cleanable(true)
                                .prefix(Icon::new(IconName::Search).small()),
                        ),
                    )
                    .child(
                        Button::new("new-skill")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .tooltip("New skill")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_new_skill_dialog(window, cx)
                            })),
                    )
                    .child(
                        Button::new("refresh")
                            .ghost()
                            .small()
                            .icon(IconName::RotateCw)
                            .tooltip("Re-scan every scope")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.rescan(None, window, cx)),
                            ),
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
                    .child(body),
            )
            .when_some(warnings, |this, warnings| {
                // Discovery never stops on an unreadable directory, but it
                // should not stay quiet about one either.
                let detail = warnings.join("\n");
                this.child(
                    h_flex()
                        .flex_shrink_0()
                        .h_8()
                        .px_3()
                        .gap_2()
                        .items_center()
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(
                            Icon::new(IconName::TriangleAlert)
                                .xsmall()
                                .text_color(cx.theme().warning),
                        )
                        .child(
                            div()
                                .id("scan-warnings")
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .tooltip(move |window, cx| {
                                    Tooltip::new(detail.clone()).build(window, cx)
                                })
                                .child(format!(
                                    "{} problem{} during the scan",
                                    warnings.len(),
                                    if warnings.len() == 1 { "" } else { "s" }
                                )),
                        ),
                )
            })
    }

    fn render_skill_row(
        &self,
        skill: &SkillView,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = skill.name.clone();
        let agents: Vec<&'static str> = skill.visible_to.iter().copied().map(agent_label).collect();
        let invalid = skill.parse_error.is_some();
        let subtitle = if invalid {
            skill
                .parse_error
                .clone()
                .unwrap_or_else(|| "Could not be parsed.".into())
        } else if skill.description.is_empty() {
            "No description — the frontmatter is missing it.".into()
        } else {
            skill.description.clone()
        };

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
                    .when(!skill.conflicts.is_empty(), |this| {
                        this.child(
                            Icon::new(IconName::Copy)
                                .xsmall()
                                .text_color(cx.theme().muted_foreground),
                        )
                    })
                    .when(invalid, |this| {
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
                    .truncate()
                    .text_color(if invalid {
                        cx.theme().warning
                    } else {
                        cx.theme().muted_foreground
                    })
                    .child(subtitle),
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

    /// Ask for a name and a description, then write a templated `SKILL.md`
    /// into the store and select what was created.
    fn open_new_skill_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_name
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.new_description
            .update(cx, |state, cx| state.set_value("", window, cx));

        let name_state = self.new_name.clone();
        let description_state = self.new_description.clone();
        let this = cx.entity().downgrade();

        window.open_dialog(cx, move |dialog, _, _| {
            let fields = (name_state.clone(), description_state.clone());
            let this = this.clone();
            dialog
                .title("New skill")
                .width(px(460.))
                .content(move |content, _, cx| {
                    let (name, description) = &fields;
                    content.child(
                        v_flex()
                            .p_4()
                            .gap_4()
                            .child(
                                v_flex()
                                    .gap_2()
                                    .child(Label::new("Name"))
                                    .child(Input::new(name).small())
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(
                                                "kebab-case. It becomes the directory name in \
                                                 ~/.skillbase/store.",
                                            ),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .gap_2()
                                    .child(Label::new("Description"))
                                    .child(Textarea::new(description).h(rems(4.))),
                            ),
                    )
                })
                // A `Dialog` renders only the footer it is given; the button
                // props are the alert dialog's affair.
                .footer(
                    DialogFooter::new()
                        .p_4()
                        .child(
                            DialogClose::new()
                                .child(Button::new("cancel-new-skill").outline().label("Cancel")),
                        )
                        .child(
                            Button::new("confirm-new-skill")
                                .primary()
                                .label("Create")
                                .on_click(move |_, window, cx| {
                                    this.update(cx, |this, cx| this.create_skill(window, cx))
                                        .ok();
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    fn create_skill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.new_name.read(cx).value().trim().to_string();
        let description = self.new_description.read(cx).value().trim().to_string();
        let roots = self.roots.clone();
        let selected = SharedString::from(name.clone());

        cx.spawn_in(window, async move |this, cx| {
            let created = cx
                .background_spawn(async move { Installer::new(roots).create(&name, &description) })
                .await;
            this.update_in(cx, |this, window, cx| {
                let outcome = created.map(|(_, outcome)| outcome);
                if report("New skill", outcome, window, cx) {
                    this.rescan(Some(selected), window, cx);
                }
            })
            .ok();
        })
        .detach();
    }
}

fn empty_state(
    title: &'static str,
    detail: SharedString,
    cx: &mut Context<Skillbase>,
) -> AnyElement {
    v_flex()
        .py_8()
        .px_2()
        .gap_1()
        .child(div().text_sm().child(title))
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(detail),
        )
        .into_any_element()
}
