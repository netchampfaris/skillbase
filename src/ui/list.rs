//! The middle column: a band naming the scope and ordering it, a search field,
//! and a scrolling list of skill rows.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::label::Label;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, InteractiveElementExt as _, Sizable as _, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, ElementId, InteractiveElement as _, IntoElement,
    KeyBinding, ParentElement as _, ScrollHandle, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, actions, div, px, rems,
};
use skillbase_core::Installer;

use crate::app::{ScanState, Skillbase};

use super::model::{Library, SkillSort, SkillView};
use super::report;

/// A comfortable default: long enough for a skill name plus a description
/// fragment, short enough that the detail pane keeps the surplus.
pub const LIST_WIDTH: f32 = 320.;
pub const LIST_MIN_WIDTH: f32 = 240.;
pub const LIST_MAX_WIDTH: f32 = 460.;

/// The keymap context the list's own bindings live in.
const CONTEXT: &str = "SkillList";

actions!(
    skill_list,
    [
        SelectNext,
        SelectPrev,
        SelectFirst,
        SelectLast,
        ConfirmSelection
    ]
);

/// Register the keys that drive the list, and nothing else.
///
/// The list is one tab stop with the arrows moving the selection inside it,
/// which is what Finder and Mail do and what a row-per-tab-stop list would
/// make unusable: fifty skills would be fifty stops between the search field
/// and the detail pane. Every binding is scoped to [`CONTEXT`], so the arrows
/// keep their ordinary meaning everywhere else — inside the search field
/// above the list, for one.
///
/// Called once during startup, before the menu bar snapshots the keymap.
pub(crate) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("down", SelectNext, Some(CONTEXT)),
        KeyBinding::new("up", SelectPrev, Some(CONTEXT)),
        KeyBinding::new("home", SelectFirst, Some(CONTEXT)),
        KeyBinding::new("end", SelectLast, Some(CONTEXT)),
        KeyBinding::new("enter", ConfirmSelection, Some(CONTEXT)),
    ]);
}

impl Skillbase {
    pub(crate) fn render_skill_list(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let query = self.search.read(cx).value();
        let warnings = self
            .scan()
            .map(|scan| scan.warnings.clone())
            .filter(|warnings| !warnings.is_empty());

        // The list's own focus, kept beside the scroll position rather than on
        // the view: it belongs to this column, and the view already owns the
        // window's root handle. Both are keyed state so they survive the
        // frames the column is rendered in.
        let focus = window
            .use_keyed_state("skill-list-focus", cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone();
        let scroll = window
            .use_keyed_state("skill-list-scroll", cx, |_, _| ScrollHandle::default())
            .read(cx)
            .clone();
        let list_focused = focus.is_focused(window);

        // The order the rows are rendered in, which is the order the arrows
        // walk. Collected here rather than re-derived in the key handlers, so
        // the filter and the sort cannot mean one thing to the eye and another
        // to the keyboard.
        let mut ordered: Vec<SharedString> = Vec::new();

        let body: Vec<AnyElement> = match &self.scan {
            ScanState::Loading => vec![
                v_flex()
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
            ],
            ScanState::Failed(error) => vec![empty_state("Could not scan", error.clone(), cx)],
            ScanState::Ready(scan) => {
                let mut matches = scan.filter(self.scope, &query);
                if self.preferences.sort == SkillSort::MostUsed {
                    // Ties fall back to the name, so the order is stable rather
                    // than whatever the filter happened to produce — and every
                    // skill nothing has recorded is a tie at zero, which on a
                    // typical machine is most of them.
                    matches.sort_by(|a, b| {
                        self.usage_count(&b.name)
                            .cmp(&self.usage_count(&a.name))
                            .then_with(|| a.name.cmp(&b.name))
                    });
                }
                if matches.is_empty() {
                    // Both states name the way out rather than only reporting
                    // the absence: one points at the two commands that put a
                    // skill here, the other at the search that is hiding them.
                    vec![if query.is_empty() {
                        empty_state(
                            "No skills here",
                            "Install one from GitHub, or create one with New skill in the \
                             sidebar."
                                .into(),
                            cx,
                        )
                    } else {
                        empty_state(
                            "Nothing matched",
                            format!(
                                "No skill here matches “{query}”. Clear the search to see all {}.",
                                scan.count(self.scope)
                            )
                            .into(),
                            cx,
                        )
                    }]
                } else {
                    // A count is shown only when it is what the order is based
                    // on. Sorted by name it is a number with nothing to do.
                    let counts = self.preferences.sort == SkillSort::MostUsed;
                    ordered = matches.iter().map(|skill| skill.name.clone()).collect();
                    matches
                        .into_iter()
                        .map(|skill| {
                            let selected = self.selected.as_ref() == Some(&skill.name);
                            self.render_skill_row(skill, selected, list_focused, counts, cx)
                        })
                        .collect()
                }
            }
        };
        let ordered = Rc::new(ordered);

        // What the column is showing, and how much of it there is. `None`
        // until the first scan lands, so the header does not claim zero.
        let total = self.scan().map(|scan| scan.count(self.scope));
        let scope_row = h_flex()
            .h_full()
            .w_full()
            // 20pt, which is where the rows' text starts: the scroll container
            // insets by 8 and each row by another 12. The band, the search
            // field and the rows are one column and read as one spine.
            .px_5()
            .gap_2()
            .items_center()
            .children(self.sidebar_reopen(cx))
            // The size every column's band title uses. The list is a peer of
            // the detail, settings and Discover bands, not a subordinate of
            // them.
            .child(div().text_base().font_medium().child(self.scope.title()))
            .child(match total {
                Some(total) => div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(total.to_string())
                    .into_any_element(),
                // The same placeholder the rows below are wearing while the
                // scan runs, rather than a third idiom for the one moment. A
                // dash reads as a value — zero, or not applicable — which is
                // the wrong thing to say about a number that is only late. It
                // is sized to a two-digit count so the band does not reflow
                // when the real one arrives.
                None => Skeleton::new()
                    .h(rems(0.8))
                    .w(rems(1.25))
                    .into_any_element(),
            })
            .child(div().flex_1().min_w_0())
            .child(self.sort_menu(cx))
            .child(
                Button::new("library-help")
                    .ghost()
                    .small()
                    .icon(IconName::Info)
                    .tooltip("What Shared, Managed and the other groups mean")
                    // A tooltip is not an accessible name, so an icon-only
                    // button has to be given one as well.
                    .accessibility_label("What the library groups mean")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_library_help(window, cx)),
                    ),
            );

        v_flex()
            .size_full()
            .min_w_0()
            .bg(cx.theme().background)
            .child(self.column_band("skill-list-band", scope_row, window, cx))
            .child(
                // Same 20pt spine as the band above and the rows below.
                h_flex().flex_shrink_0().h_11().px_5().items_center().child(
                    div().flex_1().min_w_0().child(
                        Input::new(&self.search)
                            .small()
                            .cleanable(true)
                            .prefix(Icon::new(IconName::Search).small()),
                    ),
                ),
            )
            .child(
                // The scroll area is built by hand rather than with
                // `overflow_y_scrollbar`, which wraps the caller's element in
                // a scroll area of its own. A `ScrollHandle` records the
                // bounds of its element's *direct* children, and
                // `scroll_to_item` addresses them by index — so the rows have
                // to be the scrolling element's own children for the keyboard
                // to be able to bring one into view.
                div()
                    .id("skill-list")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .id("skill-list-rows")
                            // One tab stop for the whole list. GPUI delivers a
                            // key only to the element that holds focus, so
                            // without a tracked handle here nothing the list
                            // binds would ever fire.
                            .key_context(CONTEXT)
                            .track_focus(&focus)
                            .tab_stop(true)
                            .track_scroll(&scroll)
                            .size_full()
                            .overflow_y_scroll()
                            // Otherwise gpui folds a horizontal swipe onto the
                            // one axis this area scrolls.
                            .lock_scroll_axis()
                            .px_2()
                            .pb_2()
                            .on_action({
                                let ordered = ordered.clone();
                                let scroll = scroll.clone();
                                cx.listener(move |this, _: &SelectNext, window, cx| {
                                    let next = match this.selection_index(&ordered) {
                                        Some(index) => index + 1,
                                        None => 0,
                                    };
                                    this.move_selection(&ordered, next, &scroll, window, cx);
                                })
                            })
                            .on_action({
                                let ordered = ordered.clone();
                                let scroll = scroll.clone();
                                cx.listener(move |this, _: &SelectPrev, window, cx| {
                                    let previous = match this.selection_index(&ordered) {
                                        Some(index) => index.saturating_sub(1),
                                        // Nothing selected: Up enters the list
                                        // from its end, as Down enters it from
                                        // the top.
                                        None => ordered.len().saturating_sub(1),
                                    };
                                    this.move_selection(&ordered, previous, &scroll, window, cx);
                                })
                            })
                            .on_action({
                                let ordered = ordered.clone();
                                let scroll = scroll.clone();
                                cx.listener(move |this, _: &SelectFirst, window, cx| {
                                    this.move_selection(&ordered, 0, &scroll, window, cx);
                                })
                            })
                            .on_action({
                                let ordered = ordered.clone();
                                let scroll = scroll.clone();
                                cx.listener(move |this, _: &SelectLast, window, cx| {
                                    let last = ordered.len().saturating_sub(1);
                                    this.move_selection(&ordered, last, &scroll, window, cx);
                                })
                            })
                            .on_action({
                                let ordered = ordered.clone();
                                let scroll = scroll.clone();
                                // The arrows already open each row as they
                                // reach it, so Enter has nothing new to open.
                                // What it does is bring the selection back
                                // into view, and settle a list whose selection
                                // the last filter left behind.
                                cx.listener(move |this, _: &ConfirmSelection, window, cx| {
                                    let current = this.selection_index(&ordered).unwrap_or(0);
                                    this.move_selection(&ordered, current, &scroll, window, cx);
                                })
                            })
                            .children(body),
                    )
                    .vertical_scrollbar(&scroll),
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

    /// Where the current selection sits among the rows the list is showing.
    ///
    /// `None` when nothing is selected, and also when the selected skill is
    /// not one of these rows — the search can hide the selection without
    /// clearing it, and the next arrow key should then enter the list from an
    /// end rather than from a row that is not on screen.
    fn selection_index(&self, ordered: &[SharedString]) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        ordered.iter().position(|name| name == selected)
    }

    /// Select the row at `index`, clamped to the list, and bring it into view.
    fn move_selection(
        &mut self,
        ordered: &[SharedString],
        index: usize,
        scroll: &ScrollHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let index = index.min(ordered.len().saturating_sub(1));
        let Some(name) = ordered.get(index) else {
            return;
        };
        // Asked for before the selection changes, because the handle applies
        // it during the next prepaint — the same frame the new selection is
        // painted in.
        scroll.scroll_to_item(index);
        // The click path, so a selection made with the keyboard and one made
        // with the mouse cannot come to mean different things.
        self.select_skill(name.clone(), window, cx);
        // `select_skill` returns early when the name has not changed, and at
        // either end of the list that is every keystroke. The scroll still has
        // to be drawn.
        cx.notify();
    }

    /// The ordering control.
    ///
    /// A menu rather than a pair of buttons: the two orderings are one choice.
    /// Sorting is a property of what the column is showing, so it sits in the
    /// band that names it.
    fn sort_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.preferences.sort;
        let this = cx.entity().downgrade();

        Button::new("sort")
            .ghost()
            .small()
            .icon(IconName::SortDescending)
            .tooltip("Sort the list")
            .accessibility_label("Sort the list")
            .dropdown_menu(move |menu, _, _| {
                let this = this.clone();
                let mut menu = menu.min_w(px(148.));
                for sort in SkillSort::ALL {
                    let this = this.clone();
                    menu = menu.item(
                        PopupMenuItem::new(sort.label())
                            .checked(sort == current)
                            .on_click(move |_, window, cx| {
                                this.update(cx, |this, cx| this.set_sort(sort, window, cx))
                                    .ok();
                            }),
                    );
                }
                menu
            })
    }

    /// What the Library group rows mean, and how a skill lands in each.
    fn open_library_help(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.open_dialog(cx, |dialog, _, _| {
            dialog
                .title("Library groups")
                .width(px(420.))
                .content(|content, _, cx| {
                    content.child(
                        v_flex().p_4().gap_4().children(
                            Library::ALL
                                .into_iter()
                                .filter(|library| *library != Library::All)
                                .map(|library| {
                                    v_flex()
                                        .gap_1()
                                        .child(div().text_sm().font_medium().child(library.label()))
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(library.explanation()),
                                        )
                                }),
                        ),
                    )
                })
                .footer(
                    DialogFooter::new().p_4().child(
                        DialogClose::new()
                            .child(Button::new("close-library-help").outline().label("Close")),
                    ),
                )
        });
    }

    /// One row. `list_focused` is the column's own focus, not the row's: the
    /// list is a single tab stop, so what a focus treatment has to say is
    /// which row the arrow keys are about to move away from.
    fn render_skill_row(
        &self,
        skill: &SkillView,
        selected: bool,
        list_focused: bool,
        counts: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = skill.name.clone();
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
            // Every row carries the edge, transparent unless it is the
            // selection, so gaining one does not shift the text by a pixel.
            .border_1()
            .border_color(cx.theme().transparent)
            .when(selected, |this| {
                this.bg(cx.theme().list_active)
                    // A background lightness alone is not a difference every
                    // display, or every reader, resolves — so the selection is
                    // outlined as well. While the list has focus that outline
                    // takes the focus colour, which is what separates "this is
                    // the selection" from "this is the selection and the arrow
                    // keys are going here".
                    .border_color(if list_focused {
                        cx.theme().ring
                    } else {
                        cx.theme().list_active_border
                    })
            })
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
                    // Quiet, in the same lane as the copy and warning marks.
                    // An update is not a problem with the skill, so it is not
                    // coloured like one; it is a fact about the copy on disk,
                    // and the detail pane is where the sentence for it lives.
                    .when(self.has_update(&skill.name), |this| {
                        this.child(
                            div()
                                .id(ElementId::from((
                                    ElementId::from("update-marker"),
                                    skill.name.clone(),
                                )))
                                .flex_shrink_0()
                                .tooltip(|window, cx| {
                                    Tooltip::new("An update is available").build(window, cx)
                                })
                                .child(
                                    Icon::new(IconName::ArrowDown)
                                        .xsmall()
                                        .text_color(cx.theme().muted_foreground),
                                ),
                        )
                    })
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
                    })
                    .when(counts, |this| {
                        let used = self.usage_count(&skill.name);
                        this.child(
                            div()
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(if used == 0 {
                                    SharedString::from("—")
                                } else {
                                    SharedString::from(used.to_string())
                                }),
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
            .into_any_element()
    }

    /// Ask for a name and a description, then write a templated `SKILL.md`
    /// into the store and select what was created.
    pub(crate) fn open_new_skill_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                if report(
                    "New skill",
                    "Could not create",
                    outcome,
                    &this.roots,
                    window,
                    cx,
                ) {
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
        // The spine the row text sits on: the scroll container has already
        // inset by 8, and a row adds 12.
        .px_3()
        .gap_1()
        // Weight, not colour alone, is what makes the first line the title of
        // the second.
        .child(div().text_sm().font_medium().child(title))
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(detail),
        )
        .into_any_element()
}
