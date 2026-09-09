//! The middle column: a band naming the scope and ordering it, a search field,
//! and a scrolling list of skill rows.

use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogButtonProps, DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::label::Label;
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, InteractiveElementExt as _, Sizable as _,
    StyledExt as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AnyView, App, AppContext as _, ClickEvent, Context, ElementId, FocusHandle,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, ScrollHandle,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, actions, div, px, rems,
};
use skillbase_core::{
    AgentDef, DeletePlan, InstallError, Installer, MAX_NAME_LEN, MIN_QUERY_LEN, Outcome, Registry,
    RemoteCache, is_kebab_case,
};

use crate::app::{ScanState, Skillbase};
use crate::menus::{ClearMarks, MarkAll};

use super::model::{Library, Scan, Scope, SkillSort, SkillView, display_path, join_and};
use super::{
    agent_icon, delete_cache_failure_notification, delete_effect, push_notice,
    remember_delete_cache_failure, report, report_delete, take_delete_cache_failure,
};

/// A comfortable default: long enough for a skill name plus a description
/// fragment, short enough that the detail pane keeps the surplus.
pub const LIST_WIDTH: f32 = 320.;
pub const LIST_MIN_WIDTH: f32 = 240.;
pub const LIST_MAX_WIDTH: f32 = 460.;

/// How wide a tooltip carrying a sentence is allowed to get.
///
/// A tooltip is read in one glance, so the line has to be short enough that the
/// eye finds the start of the next one — around fifty characters here. It is
/// also the width of the column it sits beside, which keeps a tooltip from
/// covering the list it explains.
const TOOLTIP_WIDTH: f32 = 320.;

/// The keymap context the list's own bindings live in.
///
/// Named here and used from [`crate::menus`] too: the marking commands appear
/// in the menu bar, and a menu item shows its shortcut only when the binding
/// was registered before the menu was built.
pub(crate) const CONTEXT: &str = "SkillList";

/// The keymap context the search field above the list lives in.
///
/// Separate from [`CONTEXT`] because the two want opposite things from the
/// arrows: in the list they move the selection, in a text field they move the
/// caret. Only the one key that has to cross the join is bound here.
const SEARCH_CONTEXT: &str = "SkillSearch";

actions!(
    skill_list,
    [
        SelectNext,
        SelectPrev,
        SelectFirst,
        SelectLast,
        ExtendNext,
        ExtendPrev,
        ConfirmSelection,
        EnterList
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
        // Shift with an arrow grows the marked set instead of moving the one
        // selection, which is what every desktop list does and what makes a
        // run of rows reachable without the mouse. The detail pane stays where
        // it is: extending a mark is not opening a skill.
        KeyBinding::new("shift-down", ExtendNext, Some(CONTEXT)),
        KeyBinding::new("shift-up", ExtendPrev, Some(CONTEXT)),
        // The join Cmd-F leaves open: the field takes focus, and without
        // these two the arrows and Enter stop at its edge. Down is what
        // Spotlight and Finder use to step from a search field into its
        // results; Enter is what a reader tries when Down does not occur to
        // them. A single-line input leaves both keys unhandled, so the
        // deeper binding runs first and then hands them on.
        KeyBinding::new("down", EnterList, Some(SEARCH_CONTEXT)),
        KeyBinding::new("enter", EnterList, Some(SEARCH_CONTEXT)),
    ]);
}

/// The rows a bulk action works on, and the two ends a range extension is
/// measured between.
///
/// It sits beside the single selection rather than replacing it: the selection
/// is what the detail pane shows, and one skill open in an editor is a
/// different thing from six skills about to be deleted. A plain click sets both
/// to the same row, so a set of one is the interface that was here before.
///
/// Every position is worked out against the rows the list is showing *now* —
/// filtered by the search field and ordered by the sort menu — so shift never
/// marks a row the reader cannot see. Membership is by name, because a scan, a
/// filter or a re-sort moves every index.
#[derive(Clone, Debug, Default)]
pub(crate) struct Marks {
    /// Every marked skill, by name.
    names: Vec<SharedString>,
    /// Where a range extension starts: the last row plain-clicked or toggled.
    anchor: Option<SharedString>,
    /// The far end of the last extension, which is where a shift-arrow carries
    /// on from.
    cursor: Option<SharedString>,
}

impl Marks {
    /// Mark this row and nothing else, and start measuring ranges from it.
    ///
    /// What a plain click and every arrow key do.
    pub(crate) fn reset_to(&mut self, name: Option<SharedString>) {
        self.names = name.iter().cloned().collect();
        self.anchor = name.clone();
        self.cursor = name;
    }

    /// Add this row to the marked set, or take it out again.
    ///
    /// The row becomes the anchor either way: the next shift-click measures
    /// from the row the user last touched, marked or not, which is what
    /// Finder does.
    pub(crate) fn toggle(&mut self, name: SharedString) {
        match self.names.iter().position(|marked| *marked == name) {
            Some(at) => {
                self.names.remove(at);
            }
            None => self.names.push(name.clone()),
        }
        self.anchor = Some(name.clone());
        self.cursor = Some(name);
    }

    /// Mark every row between the anchor and `name`, in the order the list is
    /// showing.
    ///
    /// An anchor the current filter has hidden is not one end of a visible
    /// range, so the gesture falls back to marking the row that was clicked.
    /// Extending to rows nobody can see is worse than doing less.
    pub(crate) fn extend_to(&mut self, name: SharedString, ordered: &[SharedString]) {
        let to = index_of(ordered, &name);
        let from = self.anchor.as_ref().and_then(|at| index_of(ordered, at));
        match (from, to) {
            (Some(from), Some(to)) => {
                let (first, last) = if from <= to { (from, to) } else { (to, from) };
                self.names = ordered[first..=last].to_vec();
                self.cursor = Some(name);
            }
            _ => self.reset_to(Some(name)),
        }
    }

    /// Where a shift-arrow lands, having marked everything from the anchor to
    /// there.
    ///
    /// `from` is where the list's own selection sits, used when no extension
    /// has started yet. `None` means the list has no rows to move through.
    pub(crate) fn step(
        &mut self,
        ordered: &[SharedString],
        forward: bool,
        from: Option<usize>,
    ) -> Option<usize> {
        if ordered.is_empty() {
            return None;
        }
        let at = self
            .cursor
            .as_ref()
            .and_then(|at| index_of(ordered, at))
            .or(from);
        let next = match (at, forward) {
            (Some(at), true) => (at + 1).min(ordered.len() - 1),
            (Some(at), false) => at.saturating_sub(1),
            // Nothing to extend from: the key enters the list at the end it
            // points away from, exactly as the plain arrows do.
            (None, true) => 0,
            (None, false) => ordered.len() - 1,
        };
        let name = ordered[next].clone();
        if self.anchor.is_none() {
            self.anchor = Some(name.clone());
        }
        self.extend_to(name, ordered);
        Some(next)
    }

    /// Mark every row the list is showing.
    pub(crate) fn mark_all(&mut self, ordered: &[SharedString]) {
        self.names = ordered.to_vec();
        self.anchor = ordered.first().cloned();
        self.cursor = ordered.last().cloned();
    }

    /// Drop the marks whose skills are no longer there.
    ///
    /// Called after every scan: a deleted skill that stayed marked would put a
    /// name in the band's count that nothing on disk backs up.
    pub(crate) fn retain(&mut self, present: impl Fn(&SharedString) -> bool) {
        self.names.retain(&present);
        self.anchor = self.anchor.take().filter(&present);
        self.cursor = self.cursor.take().filter(&present);
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.names.iter().any(|marked| marked == name)
    }

    pub(crate) fn len(&self) -> usize {
        self.names.len()
    }

    pub(crate) fn names(&self) -> &[SharedString] {
        &self.names
    }
}

fn index_of(ordered: &[SharedString], name: &SharedString) -> Option<usize> {
    ordered.iter().position(|row| row == name)
}

/// The rows the list shows, in the order it shows them.
///
/// One function rather than a filter in `render` and a second one in the key
/// handlers: the arrows, shift-extension, Mark all and the bulk actions all
/// have to mean the rows on screen, and a second copy of this order is a second
/// chance for them to disagree.
pub(crate) fn listed<'a>(
    scan: &'a Scan,
    scope: Scope,
    query: &str,
    sort: SkillSort,
    has_update: &dyn Fn(&str) -> bool,
    usage_count: &dyn Fn(&str) -> u32,
) -> Vec<&'a SkillView> {
    let mut matches = scan.filter(scope, query, has_update);
    if sort == SkillSort::MostUsed {
        // Ties fall back to the name, so the order is stable rather than
        // whatever the filter happened to produce — and every skill nothing has
        // recorded is a tie at zero, which on a typical machine is most of them.
        matches.sort_by(|a, b| {
            usage_count(&b.name)
                .cmp(&usage_count(&a.name))
                .then_with(|| a.name.cmp(&b.name))
        });
    }
    matches
}

/// What one row needs to know about its own state, gathered so the row builder
/// takes a state and not seven booleans.
#[derive(Clone, Copy)]
struct RowState<'a> {
    /// The one skill the detail pane is showing.
    selected: bool,
    marked: bool,
    /// True while more than one row is marked, which is when the check column
    /// and the band above the list appear.
    marking: bool,
    /// The column's own focus, not the row's: the list is a single tab stop,
    /// so what a focus treatment has to say is which row the arrow keys are
    /// about to move away from.
    list_focused: bool,
    /// Whether the invocation count is part of the ordering, and so worth
    /// showing.
    counts: bool,
    focus: &'a FocusHandle,
    /// The rows on screen, so a shift-click can measure a range against them.
    ordered: &'a Rc<Vec<SharedString>>,
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
        // walk and the order a shift-click measures a range against. Collected
        // here rather than re-derived in the key handlers, so the filter and
        // the sort cannot mean one thing to the eye and another to the
        // keyboard.
        let mut ordered: Rc<Vec<SharedString>> = Rc::new(Vec::new());
        // How many rows the list is actually showing, which is what the count
        // in the band has to report. Taken from the same pass that builds the
        // rows rather than derived a second time, so the search cannot hide a
        // row the number still counts. `None` until a scan has landed.
        let mut shown: Option<usize> = None;

        let body: Vec<AnyElement> = match &self.scan {
            ScanState::Loading => vec![
                v_flex()
                    .px_3()
                    .py_2()
                    .gap_6()
                    .child(
                        // What is happening, and the one thing a cautious
                        // reader wants to know about a tool that has just
                        // walked their home directory.
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("Reading every scope. Scanning only reads; nothing on disk changes."),
                    )
                    .child(
                        v_flex().gap_4().children((0..6).map(|row| {
                            v_flex()
                                .id(ElementId::from(("skeleton", row as usize)))
                                .gap_2()
                                .child(Skeleton::new().h(rems(0.9)).w(rems(9.)))
                                .child(Skeleton::new().h(rems(0.8)).w_full())
                        })),
                    )
                    .into_any_element(),
            ],
            ScanState::Failed(error) => {
                vec![empty_state("Could not scan", error.clone(), None, cx)]
            }
            ScanState::Ready(scan) => {
                let matches = listed(
                    scan,
                    self.scope,
                    &query,
                    self.preferences.sort,
                    &|name| self.has_update(name),
                    &|name| self.usage_count(name),
                );
                shown = Some(matches.len());
                if matches.is_empty() {
                    // Every state names a way forward rather than only
                    // reporting the absence: a machine with no skills on it
                    // gets the one command that puts one there, a group that
                    // is empty gets the rule for landing in it, and a search
                    // that matched nothing gets the registry, because "nothing
                    // matched" here often means "it is not installed yet"
                    // rather than "the search is hiding it".
                    vec![if self.scope == Scope::Library(Library::Updates)
                        && !self.checked_for_updates()
                    {
                        // The one scope whose emptiness is not a fact about
                        // the machine. Saying "no skills here" would be a
                        // different claim from "nobody has asked yet".
                        if self.checking_for_updates() {
                            empty_state(
                                "Checking GitHub",
                                "Asking what has moved on since these skills were installed."
                                    .into(),
                                None,
                                cx,
                            )
                        } else {
                            empty_state(
                                "Not checked yet",
                                "Skillbase has not asked GitHub what has moved on since these \
                                 skills were installed."
                                    .into(),
                                Some(
                                    Button::new("check-for-updates")
                                        .outline()
                                        .small()
                                        .label("Check for updates")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.check_for_updates(window, cx)
                                        })),
                                ),
                                cx,
                            )
                        }
                    } else if query.is_empty() && scan.skills.is_empty() {
                        // First launch. One thing to do, and enough about a
                        // skill and about skills.sh to make doing it a
                        // decision rather than a guess. The sidebar is beside
                        // this text and does not need reading out.
                        empty_state(
                            "No skills yet",
                            "A skill is a folder with a SKILL.md file in it that tells an agent \
                             how to do one thing. Discover searches skills.sh, a public index \
                             of skills you can install."
                                .into(),
                            Some(
                                Button::new("discover-skills")
                                    .primary()
                                    .small()
                                    .label("Discover skills")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.show_discover(window, cx)
                                    })),
                            ),
                            cx,
                        )
                    } else if query.is_empty() {
                        // The machine has skills; this group has none of them.
                        // What the reader needs is the rule for landing in it,
                        // which is the same sentence the band's title carries.
                        empty_state("No skills here", scope_explanation(self.scope), None, cx)
                    } else {
                        empty_state(
                            "Nothing matched",
                            format!(
                                "No skill here matches “{query}”. Clear the search to see all \
                                 {}, or look for one that is not installed yet.",
                                scan.count(self.scope, |name| self.has_update(name))
                            )
                            .into(),
                            self.search_registry_button(&query, cx),
                            cx,
                        )
                    }]
                } else {
                    // A count is shown only when it is what the order is based
                    // on. Sorted by name it is a number with nothing to do.
                    let counts = self.preferences.sort == SkillSort::MostUsed;
                    let marking = self.marks.len() > 1;
                    ordered = Rc::new(matches.iter().map(|skill| skill.name.clone()).collect());
                    matches
                        .into_iter()
                        .map(|skill| {
                            let state = RowState {
                                selected: self.selected.as_ref() == Some(&skill.name),
                                marked: self.marks.contains(&skill.name),
                                marking,
                                list_focused,
                                counts,
                                focus: &focus,
                                ordered: &ordered,
                            };
                            self.render_skill_row(skill, &state, cx)
                        })
                        .collect()
                }
            }
        };

        // What the column is showing, and how much of it there is. `None`
        // until the first scan lands, so the header does not claim zero.
        let total = if self.scope == Scope::Library(Library::Updates) && !self.checked_for_updates()
        {
            // Same reason as the sidebar row: before the check lands there is
            // no number to show, and a zero would say something nobody asked.
            None
        } else {
            self.scan()
                .map(|scan| scan.count(self.scope, |name| self.has_update(name)))
        };
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
            //
            // The title is also the one place the word is written large, so it
            // carries its own definition: hovering "Managed" says what makes a
            // skill managed, rather than leaving the whole vocabulary behind a
            // help button at the other end of the band.
            .child({
                let explanation = scope_explanation(self.scope);
                div()
                    .id("scope-title")
                    .text_base()
                    .font_medium()
                    .tooltip(text_tooltip(explanation))
                    .child(self.scope.title())
            })
            .child(match total {
                Some(total) => div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    // "1 of 2" while a search is hiding a row: a bare total
                    // over a shorter list is a number that contradicts what is
                    // on screen.
                    .child(count_label(shown.unwrap_or(total), total))
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
                // The commands that act on more than one row, written out.
                // Marking used to be reachable only by shift-clicking a second
                // row, which nothing on screen suggested; a menu everyone opens
                // is where a reader finds out that it exists at all. The
                // library help moved in here too, as a sentence rather than as
                // an (i) with nothing beside it.
                Button::new("list-more")
                    .ghost()
                    .small()
                    .icon(IconName::Ellipsis)
                    .tooltip("Mark several skills, and what the groups mean")
                    // A tooltip is not an accessible name, so an icon-only
                    // button has to be given one as well.
                    .accessibility_label("More list commands")
                    .dropdown_menu(self.marking_menu(None, &focus, cx)),
            );

        v_flex()
            .size_full()
            .min_w_0()
            .bg(cx.theme().background)
            .child(self.column_band("skill-list-band", scope_row, window, cx))
            .child(
                // Same 20pt spine as the band above and the rows below.
                h_flex()
                    .flex_shrink_0()
                    .h_11()
                    .px_5()
                    .items_center()
                    // Cmd-F puts the caret here; this is what lets the next
                    // key carry on into the list instead of stopping. The
                    // handler has to hang off an element the field sits
                    // inside, because an action only reaches what is on the
                    // path from the root to whatever holds focus.
                    .key_context(SEARCH_CONTEXT)
                    .on_action({
                        let ordered = ordered.clone();
                        let scroll = scroll.clone();
                        let focus = focus.clone();
                        cx.listener(move |this, _: &EnterList, window, cx| {
                            if ordered.is_empty() {
                                return;
                            }
                            this.move_selection(&ordered, 0, &scroll, &focus, window, cx);
                        })
                    })
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.search)
                                .small()
                                .cleanable(true)
                                .prefix(Icon::new(IconName::Search).small()),
                        ),
                    ),
            )
            .children(self.marked_band(cx))
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
                    // How the render tests find out whether this column reached
                    // the screen. A no-op in release builds.
                    .debug_selector(|| "skill-list".into())
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
                                let focus = focus.clone();
                                cx.listener(move |this, _: &SelectNext, window, cx| {
                                    let next = match this.selection_index(&ordered) {
                                        Some(index) => index + 1,
                                        None => 0,
                                    };
                                    this.move_selection(
                                        &ordered, next, &scroll, &focus, window, cx,
                                    );
                                })
                            })
                            .on_action({
                                let ordered = ordered.clone();
                                let scroll = scroll.clone();
                                let focus = focus.clone();
                                cx.listener(move |this, _: &SelectPrev, window, cx| {
                                    let previous = match this.selection_index(&ordered) {
                                        Some(index) => index.saturating_sub(1),
                                        // Nothing selected: Up enters the list
                                        // from its end, as Down enters it from
                                        // the top.
                                        None => ordered.len().saturating_sub(1),
                                    };
                                    this.move_selection(
                                        &ordered, previous, &scroll, &focus, window, cx,
                                    );
                                })
                            })
                            .on_action({
                                let ordered = ordered.clone();
                                let scroll = scroll.clone();
                                let focus = focus.clone();
                                cx.listener(move |this, _: &SelectFirst, window, cx| {
                                    this.move_selection(&ordered, 0, &scroll, &focus, window, cx);
                                })
                            })
                            .on_action({
                                let ordered = ordered.clone();
                                let scroll = scroll.clone();
                                let focus = focus.clone();
                                cx.listener(move |this, _: &SelectLast, window, cx| {
                                    let last = ordered.len().saturating_sub(1);
                                    this.move_selection(
                                        &ordered, last, &scroll, &focus, window, cx,
                                    );
                                })
                            })
                            .on_action({
                                let ordered = ordered.clone();
                                let scroll = scroll.clone();
                                let focus = focus.clone();
                                cx.listener(move |this, _: &ExtendNext, window, cx| {
                                    this.extend_marks(&ordered, true, &scroll, &focus, window, cx);
                                })
                            })
                            .on_action({
                                let ordered = ordered.clone();
                                let scroll = scroll.clone();
                                let focus = focus.clone();
                                cx.listener(move |this, _: &ExtendPrev, window, cx| {
                                    this.extend_marks(&ordered, false, &scroll, &focus, window, cx);
                                })
                            })
                            // Mark all and Clear are bound to this context, so
                            // the keys reach them only while the list has
                            // focus. The menu items are dispatched along
                            // whatever holds focus instead, and are registered
                            // on the Skillbase root; both routes end in the
                            // same two methods.
                            .on_action(cx.listener(|this, _: &MarkAll, _, cx| {
                                this.mark_all(cx);
                            }))
                            .on_action(cx.listener(|this, _: &ClearMarks, _, cx| {
                                this.clear_marks(cx);
                            }))
                            .on_action({
                                let ordered = ordered.clone();
                                let scroll = scroll.clone();
                                // The arrows already open each row as they
                                // reach it, so Enter has nothing new to open.
                                // What it does is bring the selection back
                                // into view, and settle a list whose selection
                                // the last filter left behind.
                                let focus = focus.clone();
                                cx.listener(move |this, _: &ConfirmSelection, window, cx| {
                                    let current = this.selection_index(&ordered).unwrap_or(0);
                                    this.move_selection(
                                        &ordered, current, &scroll, &focus, window, cx,
                                    );
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
                                .tooltip(text_tooltip(detail))
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
    ///
    /// Takes the list's focus handle and claims it. Every route to a selection
    /// ends here — the arrows, Enter, a click on a row, and Down out of the
    /// search field — and the arrows are bound to the list's own context, so a
    /// selection made from anywhere else has to bring focus with it or the
    /// next arrow key goes nowhere.
    fn move_selection(
        &mut self,
        ordered: &[SharedString],
        index: usize,
        scroll: &ScrollHandle,
        focus: &FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let index = index.min(ordered.len().saturating_sub(1));
        let Some(name) = ordered.get(index) else {
            return;
        };
        focus.focus(window, cx);
        // Asked for before the selection changes, because the handle applies
        // it during the next prepaint — the same frame the new selection is
        // painted in.
        scroll.scroll_to_item(index);
        // The click path, so a selection made with the keyboard and one made
        // with the mouse cannot come to mean different things.
        self.select_skill(name.clone(), window, cx);
        if self.selected.as_ref() == Some(name) {
            // `select_skill` returns early when the name has not changed, and
            // at either end of the list that is every keystroke. The marked set
            // still has to come back to this one row: an arrow key is a plain
            // selection, whatever was marked before it.
            self.marks.reset_to(Some(name.clone()));
        }
        // The scroll still has to be drawn.
        cx.notify();
    }

    /// Bring the marked set back to whatever the detail pane is showing.
    ///
    /// Called when the scope or the search changes: the rows underneath have
    /// changed, so a set measured against the old ones no longer describes
    /// anything on screen.
    pub(crate) fn reset_marks(&mut self) {
        self.marks.reset_to(self.selected.clone());
    }

    /// Cmd-click: add this row to the marked set, or take it out.
    ///
    /// The detail pane is left alone. Marking is not opening, and routing it
    /// through the selection would put the unsaved-edit question in front of a
    /// user who is only picking rows.
    fn toggle_mark(&mut self, name: SharedString, cx: &mut Context<Self>) {
        self.marks.toggle(name);
        cx.notify();
    }

    /// Shift-click: mark everything between the anchor and this row.
    fn extend_marks_to(
        &mut self,
        name: SharedString,
        ordered: &[SharedString],
        cx: &mut Context<Self>,
    ) {
        self.marks.extend_to(name, ordered);
        cx.notify();
    }

    /// Shift with an arrow key: carry the marked set one row further and
    /// scroll to it.
    fn extend_marks(
        &mut self,
        ordered: &[SharedString],
        forward: bool,
        scroll: &ScrollHandle,
        focus: &FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        focus.focus(window, cx);
        let from = self.selection_index(ordered);
        if let Some(index) = self.marks.step(ordered, forward, from) {
            // Asked for before the paint, the same way `move_selection` does.
            scroll.scroll_to_item(index);
        }
        cx.notify();
    }

    /// The rows the list is showing, in the order it shows them.
    ///
    /// The render pass collects the same order as it builds the rows and hands
    /// it to the handlers it registers. A handler registered anywhere else has
    /// no such pass to take it from, so it derives the order here from the same
    /// scope, search and sort the rows were built from.
    pub(crate) fn visible_rows(&self, cx: &App) -> Vec<SharedString> {
        let Some(scan) = self.scan() else {
            return Vec::new();
        };
        let query = self.search.read(cx).value();
        listed(
            scan,
            self.scope,
            &query,
            self.preferences.sort,
            &|name| self.has_update(name),
            &|name| self.usage_count(name),
        )
        .into_iter()
        .map(|skill| skill.name.clone())
        .collect()
    }

    /// Mark every row the list is showing.
    ///
    /// With a search in the field that is the matches and nothing else, which
    /// is what makes "find the stale ones, then remove them" one gesture.
    ///
    /// Derives the rows rather than taking them, so the menu bar's handler —
    /// which GPUI dispatches along whatever holds focus, and so cannot live on
    /// the list element — can call it with nothing but a context.
    pub(crate) fn mark_all(&mut self, cx: &mut Context<Self>) {
        let ordered = self.visible_rows(cx);
        self.marks.mark_all(&ordered);
        cx.notify();
    }

    pub(crate) fn clear_marks(&mut self, cx: &mut Context<Self>) {
        self.reset_marks();
        cx.notify();
    }

    /// The marked skills, in the order the last scan found them.
    ///
    /// Scan order rather than marking order, so a confirmation lists the same
    /// names in the same places however the set was built up.
    fn marked_skills(&self) -> Vec<&SkillView> {
        let Some(scan) = self.scan() else {
            return Vec::new();
        };
        scan.skills
            .iter()
            .filter(|skill| self.marks.contains(&skill.name))
            .collect()
    }

    /// The commands that act on a set of rows rather than on the selection.
    ///
    /// Built once and used twice: from the button in the band, where somebody
    /// who has never marked anything can read that marking exists, and from a
    /// right-click on a row, which is where a desktop reader looks for it. Both
    /// routes end in the same four methods the menu bar calls, so there is one
    /// command with one shape however it was reached.
    ///
    /// `row` is the skill the menu was opened on, when it was opened on one.
    /// `None` is the band's copy, which has no row to act on and carries the
    /// library help instead.
    ///
    /// `focus` is the list's own focus handle, given to the menu so it can find
    /// the keys these commands are bound to — they are scoped to [`CONTEXT`],
    /// and a menu with no context to search would show no shortcut at all.
    fn marking_menu(
        &self,
        row: Option<SharedString>,
        focus: &FocusHandle,
        cx: &mut Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        let this = cx.entity().downgrade();
        let focus = focus.clone();
        let marked = self.marks.len();
        // Every bulk command opens a dialog or writes, and both are refused
        // while one is already running.
        let busy = self.bulk_busy;
        // A set is two rows or more: one marked row is the selection, which the
        // detail pane is already showing.
        let has_set = marked > 1;
        let row_marked = row.as_deref().is_some_and(|name| self.marks.contains(name));
        // True for the band's copy, which acts on the column rather than on a
        // row and is the one that carries the library help.
        let in_band = row.is_none();
        // The selection is the marked set of one, so until a real set exists
        // its own row has nothing to mark or unmark and the item is left out
        // rather than offered as a command that changes nothing. Every other
        // row keeps it, and another row is what somebody meeting this feature
        // for the first time right-clicks.
        let row = row.filter(|name| has_set || self.selected.as_ref() != Some(name));

        move |menu, _, _| {
            let mut menu = menu
                .action_context(focus.clone())
                // Wide enough for "Unlink marked skills…" and its shortcut on
                // one line.
                .min_w(px(240.));

            if let Some(name) = row.clone() {
                let this = this.clone();
                menu = menu.item(
                    PopupMenuItem::new(if row_marked {
                        "Unmark this skill"
                    } else {
                        "Mark this skill"
                    })
                    .on_click(move |_, _, cx| {
                        this.update(cx, |this, cx| this.toggle_mark(name.clone(), cx))
                            .ok();
                    }),
                );
            }

            let mark_all = this.clone();
            menu = menu.item(
                PopupMenuItem::new("Mark all")
                    // The action is here for its key: the handler above it is
                    // what runs, so nothing depends on the dispatch reaching
                    // the list.
                    .action(Box::new(MarkAll))
                    .on_click(move |_, _, cx| {
                        mark_all.update(cx, |this, cx| this.mark_all(cx)).ok();
                    }),
            );

            let clear = this.clone();
            menu = menu.item(
                PopupMenuItem::new("Clear marks")
                    .action(Box::new(ClearMarks))
                    .disabled(!has_set)
                    .on_click(move |_, _, cx| {
                        clear.update(cx, |this, cx| this.clear_marks(cx)).ok();
                    }),
            );

            menu = menu.separator();

            for (label, on) in [
                ("Link marked skills…", true),
                ("Unlink marked skills…", false),
            ] {
                let this = this.clone();
                menu = menu.item(
                    PopupMenuItem::new(label)
                        .disabled(!has_set || busy)
                        .on_click(move |_, window, cx| {
                            this.update(cx, |this, cx| {
                                this.open_link_marked_dialog(on, window, cx)
                            })
                            .ok();
                        }),
                );
            }

            let delete = this.clone();
            menu = menu.item(
                PopupMenuItem::new("Delete marked skills…")
                    .disabled(!has_set || busy)
                    .on_click(move |_, window, cx| {
                        delete
                            .update(cx, |this, cx| this.confirm_delete_marked(window, cx))
                            .ok();
                    }),
            );

            // Only the band's copy. A right-click on a row is about that row,
            // and a glossary at the foot of it would be an answer to a question
            // nobody asked there.
            if in_band {
                let help = this.clone();
                menu = menu.separator().item(
                    PopupMenuItem::new("What the library groups mean").on_click(
                        move |_, window, cx| {
                            help.update(cx, |this, cx| this.open_library_help(window, cx))
                                .ok();
                        },
                    ),
                );
            }

            menu
        }
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
                // Wide enough for the sentence under "Most used". A menu that
                // wrapped it every three words would be harder to read than
                // the number it explains.
                let mut menu = menu.min_w(px(300.));
                for sort in SkillSort::ALL {
                    let this = this.clone();
                    // An element item rather than a plain one, so the ordering
                    // whose numbers need explaining can carry the explanation.
                    // Both orderings use it, because a menu whose two rows are
                    // built differently lays them out differently.
                    let description = sort.description();
                    menu = menu.item(
                        PopupMenuItem::element(move |_, cx| {
                            v_flex()
                                .gap_1()
                                .child(sort.label())
                                .children(description.clone().map(|description| {
                                    div()
                                        .max_w(rems(16.))
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(description)
                                }))
                        })
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

    /// The way out of an empty search: look for the skill where it might
    /// actually be, carrying what was typed across.
    ///
    /// `None` for a query skills.sh would refuse, so the button cannot land on
    /// a page that says "type more".
    fn search_registry_button(&self, query: &str, cx: &mut Context<Self>) -> Option<Button> {
        if query.chars().count() < MIN_QUERY_LEN {
            return None;
        }
        let query = SharedString::from(query.to_string());
        Some(
            Button::new("search-registry")
                .outline()
                .small()
                // The query is already in the sentence above; what the button
                // has to add is where it is about to look.
                .label("Search skills.sh")
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.discover_query
                        .update(cx, |state, cx| state.set_value(query.clone(), window, cx));
                    this.show_discover(window, cx);
                    // `set_value` deliberately emits nothing, so the
                    // subscription that normally starts a search does not
                    // fire. Ask for it here.
                    this.search_registry(window, cx);
                })),
        )
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

    /// The band above the list while more than one row is marked: how many
    /// there are, what can be done to all of them at once, and the way back to
    /// one.
    ///
    /// It appears with the second mark and goes away with it, so a list with a
    /// single selected row is the interface that was here before.
    fn marked_band(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let count = self.marks.len();
        if count < 2 {
            return None;
        }
        let busy = self.bulk_busy;

        Some(
            v_flex()
                .flex_shrink_0()
                // The same 20pt spine as the band, the search field and the
                // rows, so the marked state does not move the column.
                .px_5()
                .py_2()
                .gap_2()
                .bg(cx.theme().muted)
                .border_b_1()
                .border_color(cx.theme().border)
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("{count} skills marked")),
                        )
                        .child(
                            Button::new("clear-marks")
                                .ghost()
                                .xsmall()
                                .label("Clear")
                                .on_click(cx.listener(|this, _, _, cx| this.clear_marks(cx))),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            // Both open a dialog that names the agent, so both
                            // take the ellipsis.
                            Button::new("link-marked")
                                .outline()
                                .small()
                                .label("Link…")
                                .tooltip("Link every marked skill to one agent")
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_link_marked_dialog(true, window, cx)
                                })),
                        )
                        .child(
                            Button::new("unlink-marked")
                                .outline()
                                .small()
                                .label("Unlink…")
                                .tooltip("Take every marked skill's link out of one agent")
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_link_marked_dialog(false, window, cx)
                                })),
                        )
                        .child(div().flex_1().min_w_0())
                        .child(
                            // Outline rather than danger: this opens the
                            // confirmation, and the commitment is the Delete
                            // button in it.
                            Button::new("delete-marked")
                                .outline()
                                .small()
                                .label("Delete…")
                                .tooltip(
                                    "Move every marked skill to the trash and remove its links",
                                )
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.confirm_delete_marked(window, cx)
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    // ------------------------------------------------- acting on the marked set

    /// Run one filesystem operation over the whole marked set on a background
    /// thread, then report it once and scan once.
    ///
    /// One write rather than one per skill: twenty separate calls would be
    /// twenty background writes, twenty notifications and twenty full rescans,
    /// with the list rebuilt between each of them.
    fn run_on_marked<F>(
        &mut self,
        title: &'static str,
        failed: &'static str,
        op: F,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        F: FnOnce(Installer) -> Result<Outcome, InstallError> + Send + 'static,
    {
        self.run_on_marked_op(title, failed, false, op, window, cx);
    }

    /// The same, for the delete: its report carries the button that puts every
    /// skill in the set back.
    fn run_delete_marked<F>(&mut self, op: F, window: &mut Window, cx: &mut Context<Self>)
    where
        F: FnOnce(Installer) -> Result<Outcome, InstallError> + Send + 'static,
    {
        self.run_on_marked_op("Deleted", "Could not delete", true, op, window, cx);
    }

    /// The body of [`SkillList::run_on_marked`] and
    /// [`SkillList::run_delete_marked`].
    fn run_on_marked_op<F>(
        &mut self,
        title: &'static str,
        failed: &'static str,
        undoable: bool,
        op: F,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        F: FnOnce(Installer) -> Result<Outcome, InstallError> + Send + 'static,
    {
        if self.bulk_busy {
            return;
        }
        let roots = self.roots.clone();
        self.bulk_busy = true;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { op(Installer::new(roots)) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.bulk_busy = false;
                if undoable {
                    // The undo runs long after this task is over, and nothing
                    // else would notice that the directories came back, so it
                    // carries the rescan with it.
                    let entity = cx.entity().downgrade();
                    report_delete(
                        title,
                        failed,
                        result,
                        &this.roots,
                        Rc::new(move |window, cx| {
                            entity
                                .update(cx, |this, cx| this.rescan(None, window, cx))
                                .ok();
                        }),
                        window,
                        cx,
                    );
                } else {
                    report(title, failed, result, &this.roots, window, cx);
                }
                // Delete is the one operation run from here that writes the
                // remote cache, and the slot it writes into is its own, so a
                // failure waiting in it belongs to the delete just reported.
                if let Some(reason) = take_delete_cache_failure() {
                    push_notice(
                        delete_cache_failure_notification(
                            "The skills were deleted, but their install records could not be \
                             removed",
                            &reason,
                        ),
                        window,
                        cx,
                    );
                }
                // Scan again either way: a refusal still means the interface
                // should re-read what is actually there.
                this.rescan(None, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Ask which agent, then link or unlink every marked skill to it.
    ///
    /// A dialog rather than a menu because the choice needs a caption per
    /// agent: how many of the marked skills the command would actually act on
    /// is the number that decides which row to press, and a menu row has
    /// nowhere to put it. It is also what the menu bar's own item opens, so
    /// there is one command with one shape.
    pub(crate) fn open_link_marked_dialog(
        &mut self,
        on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let marked = self.marked_skills();
        if marked.len() < 2 {
            return;
        }
        let total = marked.len();
        // Linking writes into the store, so it needs an origin Skillbase owns.
        // An unmanaged skill has to be adopted first, one at a time, on its own
        // page.
        let unmanaged = marked.iter().filter(|skill| !skill.managed).count();

        let installed = self
            .scan()
            .map(|scan| scan.installed.clone())
            .unwrap_or_default();
        // Agents that read the shared directory need no link of their own, so
        // offering them here would promise a write that changes nothing.
        let choices: Vec<AgentChoice> = Registry::link_targets()
            .filter(|agent| !agent.is_shared())
            .filter(|agent| {
                installed.contains(agent) || marked.iter().any(|skill| skill.linked_to(agent.id))
            })
            .map(|agent| {
                let acts_on = marked
                    .iter()
                    .filter(|skill| {
                        if on {
                            skill.managed && !skill.linked_to(agent.id) && !skill.via_shared(agent)
                        } else {
                            skill.linked_to(agent.id)
                        }
                    })
                    .count();
                AgentChoice { agent, acts_on }
            })
            .collect();

        let this = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let choices = choices.clone();
            let this = this.clone();

            dialog
                .title(if on {
                    format!("Link {total} skills to an agent")
                } else {
                    format!("Unlink {total} skills from an agent")
                })
                .width(px(460.))
                .content(move |content, _, cx| {
                    let this = this.clone();
                    content.child(
                        v_flex()
                            .p_4()
                            .gap_2()
                            .when(on && unmanaged > 0, |column| {
                                column.child(div().text_xs().text_color(cx.theme().warning).child(
                                    format!(
                                        "{unmanaged} of them sit outside the directories \
                                         Skillbase manages. Adopt those first; they are left \
                                         alone here.",
                                    ),
                                ))
                            })
                            .when(choices.is_empty(), |column| {
                                column.child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(
                                            "No agent on this machine takes a link of its own. \
                                             Agents that read ~/.agents/skills are reached \
                                             through Shared instead.",
                                        ),
                                )
                            })
                            .children(choices.iter().map(|choice| {
                                let agent = choice.agent;
                                let acts_on = choice.acts_on;
                                let this = this.clone();
                                Button::new(ElementId::from((
                                    ElementId::from("link-marked-agent"),
                                    agent.id,
                                )))
                                .ghost()
                                .w_full()
                                .justify_start()
                                .disabled(acts_on == 0)
                                .child(
                                    h_flex()
                                        .w_full()
                                        .gap_2()
                                        .items_center()
                                        .child(agent_icon(agent).small())
                                        .child(div().text_sm().child(agent.display_name))
                                        .child(div().flex_1().min_w_0())
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(if acts_on == 0 {
                                                    if on {
                                                        "Already reaches all of them".to_string()
                                                    } else {
                                                        "Holds none of them".to_string()
                                                    }
                                                } else {
                                                    format!("{acts_on} of {total}")
                                                }),
                                        ),
                                )
                                .on_click(
                                    move |_, window, cx| {
                                        this.update(cx, |this, cx| {
                                            this.link_marked(agent, on, window, cx)
                                        })
                                        .ok();
                                        window.close_dialog(cx);
                                    },
                                )
                            })),
                    )
                })
                .footer(
                    DialogFooter::new().p_4().child(
                        DialogClose::new()
                            .child(Button::new("cancel-link-marked").outline().label("Cancel")),
                    ),
                )
        });
    }

    /// Link or unlink every marked skill to one agent, in one write.
    fn link_marked(
        &mut self,
        agent: &'static AgentDef,
        on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets: Vec<LinkTarget> = self
            .marked_skills()
            .into_iter()
            .filter(|skill| {
                if on {
                    skill.managed && !skill.linked_to(agent.id) && !skill.via_shared(agent)
                } else {
                    skill.linked_to(agent.id)
                }
            })
            .map(|skill| LinkTarget {
                name: skill.name.to_string(),
                origin: skill.origin.clone(),
                parked: skill.parked_in(agent.id),
            })
            .collect();
        if targets.is_empty() {
            return;
        }

        let (title, failed) = if on {
            ("Linked", "Could not link")
        } else {
            ("Unlinked", "Could not unlink")
        };
        self.run_on_marked(
            title,
            failed,
            move |installer| {
                let mut done = Outcome::default();
                for target in &targets {
                    // Twenty skills in one write. A refusal on the ninth still
                    // leaves the first eight linked, and the notification has
                    // to list them rather than report the refusal alone.
                    if let Err(source) = link_step(&installer, &mut done, target, agent, on) {
                        return Err(InstallError::partial(done, source));
                    }
                }
                Ok(done)
            },
            window,
            cx,
        );
    }

    /// Work out what deleting the marked skills would remove, then confirm
    /// with those paths.
    pub(crate) fn confirm_delete_marked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bulk_busy {
            return;
        }
        let discovered: Vec<_> = self
            .marked_skills()
            .into_iter()
            .map(SkillView::as_discovered)
            .collect();
        if discovered.len() < 2 {
            return;
        }
        let roots = self.roots.clone();

        cx.spawn_in(window, async move |this, cx| {
            let plans = cx
                .background_spawn(async move {
                    let installer = Installer::new(roots);
                    discovered
                        .iter()
                        .map(|skill| installer.plan_delete(skill))
                        .collect::<Vec<DeletePlan>>()
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.open_delete_marked_dialog(plans, window, cx)
            })
            .ok();
        })
        .detach();
    }

    /// One confirmation for the whole set, listing every path it would remove.
    ///
    /// The same account the single-skill dialog gives, grouped by skill: a
    /// delete is not reviewable against counts, and the user is the only one
    /// who can tell whether a path in the list belongs to them.
    fn open_delete_marked_dialog(
        &mut self,
        plans: Vec<DeletePlan>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let roots = self.roots.clone();
        let this = cx.entity().downgrade();
        let count = plans.len();

        window.open_alert_dialog(cx, move |alert, _, cx| {
            let plans = plans.clone();
            let roots = roots.clone();
            let this = this.clone();

            // The kinds are counted because a path alone does not say whether
            // it is a directory of files or a link to one; the paths are listed
            // because the counts alone cannot be checked.
            let origins = plans.iter().filter(|plan| plan.origin.is_some()).count();
            let links: usize = plans.iter().map(DeletePlan::link_count).sum();
            let copies: usize = plans.iter().map(DeletePlan::copy_count).sum();
            let mut kinds: Vec<String> = Vec::new();
            if origins > 0 {
                kinds.push(format!(
                    "{origins} skill {}",
                    if origins == 1 {
                        "directory"
                    } else {
                        "directories"
                    }
                ));
            }
            if links > 0 {
                kinds.push(format!("{links} link{}", if links == 1 { "" } else { "s" }));
            }
            if copies > 0 {
                kinds.push(format!(
                    "{copies} duplicate director{}",
                    if copies == 1 { "y" } else { "ies" }
                ));
            }
            let kind_names: Vec<&str> = kinds.iter().map(String::as_str).collect();
            let summary = if kinds.is_empty() {
                "Nothing that Skillbase manages is left to remove.".to_string()
            } else {
                format!("Removes {}:", join_and(&kind_names))
            };

            let kept: Vec<SharedString> = plans
                .iter()
                .flat_map(|plan| plan.skipped.iter())
                .map(|path| display_path(path, &roots))
                .collect();

            let description = v_flex()
                .gap_3()
                .text_sm()
                .child(div().child(summary))
                .child(
                    // Twenty skills can be a hundred paths. The list scrolls
                    // rather than pushing the buttons off the bottom of the
                    // screen.
                    div()
                        .id("delete-marked-paths")
                        .max_h(px(240.))
                        .overflow_y_scroll()
                        .child(v_flex().gap_2().children(plans.iter().map(|plan| {
                            let going: Vec<SharedString> = plan
                                .origin
                                .iter()
                                .chain(plan.links.iter())
                                .chain(plan.copies.iter())
                                .map(|path| display_path(path, &roots))
                                .collect();
                            v_flex()
                                .gap_1()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_medium()
                                        .child(SharedString::from(plan.name.clone())),
                                )
                                .children(going.into_iter().map(|path| {
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(path)
                                }))
                        }))),
                )
                // The one thing the path list cannot say: a directory is not
                // destroyed, so a mistake here is recoverable. The same
                // sentence the single-skill dialog ends on.
                .when(origins + copies > 0, |this| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(delete_effect(origins + copies)),
                    )
                })
                .when(!kept.is_empty(), |this| {
                    this.child(
                        v_flex()
                            .p_2()
                            .gap_1()
                            .rounded(cx.theme().radius)
                            .bg(cx.theme().warning.opacity(0.12))
                            .child(div().text_xs().text_color(cx.theme().warning).child(format!(
                                "{} path{} left alone, {} outside every directory Skillbase \
                                 manages:",
                                kept.len(),
                                if kept.len() == 1 { "" } else { "s" },
                                if kept.len() == 1 {
                                    "because it is"
                                } else {
                                    "because they are"
                                },
                            )))
                            .children(kept.iter().map(|path| {
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().warning)
                                    .child(path.clone())
                            })),
                    )
                });

            alert
                .title(format!("Delete {count} skills?"))
                .description(description)
                .width(px(560.))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Delete")
                        .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
                        .cancel_text("Cancel")
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    let plans = plans.clone();
                    let roots = roots.clone();
                    this.update(cx, |this, cx| {
                        this.run_delete_marked(
                            move |installer| {
                                let mut done = Outcome::default();
                                let mut stopped = None;
                                for plan in &plans {
                                    match installer.delete(plan) {
                                        Ok(outcome) => done.changes.extend(outcome.changes),
                                        Err(source) => {
                                            stopped = Some(source);
                                            break;
                                        }
                                    }
                                }
                                let result = match stopped {
                                    Some(source) => Err(InstallError::partial(done, source)),
                                    None => Ok(done),
                                };
                                // The record of a skill that is gone would
                                // answer for the next skill to take its name.
                                let mut cache = RemoteCache::read(&roots);
                                if cache.forget_deleted(&plans, &result) {
                                    remember_delete_cache_failure(cache.write(&roots));
                                }
                                result
                            },
                            window,
                            cx,
                        );
                    })
                    .ok();
                    true
                })
        });
    }

    /// One row.
    fn render_skill_row(
        &self,
        skill: &SkillView,
        state: &RowState<'_>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = skill.name.clone();
        let RowState {
            selected,
            marked,
            marking,
            list_focused,
            counts,
            focus,
            ordered,
        } = *state;
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
            // A marked row and the selected row share the highlight, which is
            // what says they are one set. What separates them is the outline:
            // the selected row is the one the detail pane is showing and the
            // one the arrows move from, and only it is outlined. The same
            // arrangement Finder uses for a multiple selection.
            .when(selected || marked, |this| this.bg(cx.theme().list_active))
            .when(selected, |this| {
                // A background lightness alone is not a difference every
                // display, or every reader, resolves — so the selection is
                // outlined as well. While the list has focus that outline
                // takes the focus colour, which is what separates "this is
                // the selection" from "this is the selection and the arrow
                // keys are going here".
                this.border_color(if list_focused {
                    cx.theme().ring
                } else {
                    cx.theme().list_active_border
                })
            })
            .when(!selected && !marked, |this| {
                this.hover(|this| this.bg(cx.theme().list_hover))
            })
            .on_click({
                let focus = focus.clone();
                let ordered = ordered.clone();
                cx.listener(move |this, event: &ClickEvent, window, cx| {
                    // The arrows are bound to the list's own context, and GPUI
                    // delivers a key only to what holds focus. Without this a
                    // row picked with the mouse leaves the keyboard pointing at
                    // whatever had focus before, and the arrows do nothing.
                    focus.focus(window, cx);
                    let modifiers = event.modifiers();
                    // Neither modifier touches the selection, so neither can
                    // put the unsaved-edit question in front of someone who is
                    // only picking rows.
                    if modifiers.secondary() {
                        this.toggle_mark(name.clone(), cx);
                    } else if modifiers.shift {
                        this.extend_marks_to(name.clone(), &ordered, cx);
                    } else {
                        this.select_skill(name.clone(), window, cx);
                        if this.selected.as_ref() == Some(&name) {
                            // `select_skill` returns early when the row is
                            // already the selection, so on its own a click on
                            // the selected row would leave a set built by
                            // shift- or cmd-clicking standing. A plain click is
                            // a plain selection, the same as an arrow key —
                            // `move_selection` compensates for the same early
                            // return.
                            this.marks.reset_to(Some(name.clone()));
                            cx.notify();
                        }
                    }
                })
            })
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .min_w_0()
                    // While a set is being built, every row reserves the same
                    // slot, so the names stay on one spine and a check is a
                    // mark rather than an indent. A tint alone would leave the
                    // marked state told by colour only.
                    .when(marking, |this| {
                        this.child(div().flex_shrink_0().size_3().child(if marked {
                            Icon::new(IconName::Check)
                                .xsmall()
                                .text_color(cx.theme().primary)
                                .into_any_element()
                        } else {
                            div().into_any_element()
                        }))
                    })
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
                                .tooltip(text_tooltip("An update is available"))
                                .child(
                                    Icon::new(IconName::ArrowDown)
                                        .xsmall()
                                        .text_color(cx.theme().muted_foreground),
                                ),
                        )
                    })
                    // Same treatment as the update marker above it: a mark that
                    // only means something once you know what it means needs a
                    // sentence within reach.
                    .when(!skill.conflicts.is_empty(), |this| {
                        let others = skill.conflicts.len();
                        this.child(
                            div()
                                .id(ElementId::from((
                                    ElementId::from("duplicate-marker"),
                                    skill.name.clone(),
                                )))
                                .flex_shrink_0()
                                .tooltip(text_tooltip(format!(
                                    "This name is a real directory in {others} other place{}",
                                    if others == 1 { "" } else { "s" }
                                )))
                                .child(
                                    Icon::new(IconName::Copy)
                                        .xsmall()
                                        .text_color(cx.theme().muted_foreground),
                                ),
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
                        if used == 0 {
                            // A dash where a number goes reads as zero, and
                            // zero reads as "never used". It is not: only two
                            // of the agents record an invocation at all, and
                            // the one that records the most throws its
                            // transcripts away. Say so where the mark is.
                            this.child(
                                div()
                                    .id(ElementId::from((
                                        ElementId::from("usage-count"),
                                        skill.name.clone(),
                                    )))
                                    .flex_shrink_0()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .tooltip(text_tooltip(
                                        "No invocation was recorded. Not every agent keeps \
                                         session records, so this is not the same as never used.",
                                    ))
                                    .child("—"),
                            )
                        } else {
                            this.child(
                                div()
                                    .flex_shrink_0()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(used.to_string()),
                            )
                        }
                    }),
            )
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .items_center()
                    // The same slot the line above reserves, so the name and
                    // the description keep one leading edge whether or not a
                    // set is being marked.
                    .when(marking, |this| this.child(div().flex_shrink_0().size_3()))
                    .child(
                        div()
                            .id(ElementId::from((
                                ElementId::from("skill-description"),
                                skill.name.clone(),
                            )))
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .truncate()
                            .text_color(if invalid {
                                cx.theme().warning
                            } else {
                                cx.theme().muted_foreground
                            })
                            // Two skills whose descriptions open with the same
                            // words truncate to the same line and stop telling
                            // each other apart. The tooltip is the rest of the
                            // sentence, and it is offered only when there is a
                            // rest — a tooltip repeating a line that already
                            // fits is noise.
                            .when_some(long_enough_to_truncate(&subtitle), |this, full| {
                                this.tooltip(text_tooltip(full))
                            })
                            .child(subtitle),
                    )
                    .child(self.reach_lane(skill, cx)),
            )
            .context_menu(self.marking_menu(Some(skill.name.clone()), focus, cx))
            .into_any_element()
    }

    /// Which agents this skill reaches, as their own marks.
    ///
    /// This is the question the application exists to answer, and until now it
    /// was two gestures away: select the row, then open a closed section in the
    /// detail pane. It sits at the trailing edge of the second line rather than
    /// beside the name, because the first line already carries the name and up
    /// to three state marks, and at the 240pt minimum width a row of logos
    /// there would push the name into truncating. On the second line it takes
    /// space the description can give up, and it forms a lane down the column
    /// that can be read without reading a word.
    fn reach_lane(&self, skill: &SkillView, cx: &mut Context<Self>) -> AnyElement {
        /// How many marks fit before the row stops being scannable. Past this
        /// the rest are counted.
        const SHOWN: usize = 4;

        let id = ElementId::from((ElementId::from("skill-reach"), skill.name.clone()));
        // Only the agents this machine actually has. `SkillView::reach` answers
        // "would this agent reach the skill", which is true of every agent that
        // reads ~/.agents/skills whether or not it is installed — so a row for
        // a shared skill drew a dozen logos on a machine the sidebar said held
        // one agent.
        let present = self.scan().map(|scan| scan.installed.as_slice());
        let agents = skill.reach(present.unwrap_or_default());
        if agents.is_empty() {
            // A skill nothing can load is a fact worth a word. The lane is at
            // the trailing edge, so saying it moves nothing else on the row.
            return div()
                .flex_shrink_0()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("No agents")
                .into_any_element();
        }

        // A logo is only a name to someone who already knows it, and a "+2"
        // names nothing at all.
        let names: Vec<&'static str> = agents.iter().map(|agent| agent.display_name).collect();
        let spoken = SharedString::from(join_and(&names));
        let extra = agents.len().saturating_sub(SHOWN);

        h_flex()
            .id(id)
            .flex_shrink_0()
            .gap_1()
            .items_center()
            .tooltip(text_tooltip(spoken))
            .children(agents.into_iter().take(SHOWN).map(|agent| {
                agent_icon(agent)
                    .xsmall()
                    .text_color(cx.theme().muted_foreground)
            }))
            .when(extra > 0, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("+{extra}")),
                )
            })
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

        // The names already on this machine, read here rather than from the
        // builder below. The builder runs from inside `Skillbase::render` —
        // that is where `Root::render_dialog_layer` is called from — so this
        // view is already borrowed by the time it runs, and reading it there
        // aborts the process.
        //
        // So the list is the one that stood when the dialog opened. A skill
        // that arrives behind an open dialog is not in it, and `Create` is the
        // backstop for that: `Installer::create` refuses a directory that is
        // already there rather than writing over it.
        let scan = self.scan().cloned();

        window.open_dialog(cx, move |dialog, _, cx| {
            let fields = (name_state.clone(), description_state.clone());
            let this = this.clone();

            // Checked on the frame the character was typed. The dialog's
            // builder runs on every frame it is on screen, and the field's own
            // state is a separate entity, so the line under the field and the
            // state of Create always describe what is in the field now rather
            // than what it held when the dialog opened.
            let typed = name_state.read(cx).value();
            let typed = typed.trim();
            let problem = name_problem(typed, scan.as_deref());
            let can_create = !typed.is_empty() && problem.is_none();
            let footer_problem = problem.clone();

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
                                        // One line, in the one place: what a
                                        // name has to look like until it does
                                        // not, and then why. A reason under
                                        // the field it is about is what stops
                                        // the dialog having to be submitted to
                                        // find out.
                                        div()
                                            .text_xs()
                                            .text_color(match &problem {
                                                Some(_) => cx.theme().danger,
                                                None => cx.theme().muted_foreground,
                                            })
                                            .child(problem.clone().unwrap_or_else(|| {
                                                "Lowercase letters, digits and single hyphens. \
                                                 It becomes the directory name in \
                                                 ~/.agents/skills."
                                                    .into()
                                            })),
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
                                // Off until the name would be accepted, with
                                // the reason showing under the field. The
                                // dialog no longer closes here: it holds the
                                // only copy of what was typed, so it closes in
                                // `create_skill`, once the file is on disk.
                                .disabled(!can_create)
                                .when_some(footer_problem, |button, problem| {
                                    button.tooltip(problem)
                                })
                                .on_click(move |_, window, cx| {
                                    this.update(cx, |this, cx| this.create_skill(window, cx))
                                        .ok();
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
                    // Only now. A refusal leaves the dialog standing with the
                    // name and the description still in it, which is the whole
                    // point of not closing beside the call.
                    window.close_dialog(cx);
                    this.rescan(Some(selected), window, cx);
                }
            })
            .ok();
        })
        .detach();
    }
}

/// One agent the marked set can be linked to or unlinked from, and how many of
/// those skills the command would actually act on.
///
/// The count is what decides which row to press: an agent that already reaches
/// all six is a row that would write nothing, and it says so rather than
/// looking available.
#[derive(Clone, Copy)]
struct AgentChoice {
    agent: &'static AgentDef,
    /// How many of the marked skills this command would change.
    acts_on: usize,
}

/// One skill a bulk link or unlink acts on, flattened so the whole batch can
/// cross onto a background thread without the scan.
struct LinkTarget {
    name: String,
    /// The real directory the link points at.
    origin: PathBuf,
    /// True when this agent's link is parked in its disabled directory, and so
    /// has to be moved back before there is anything to unlink.
    parked: bool,
}

/// Link or unlink one skill for one agent, writing what it did into `done`.
///
/// The same two steps the detail pane's own switch takes, so one skill done
/// here and one skill done there cannot come to mean different things.
fn link_step(
    installer: &Installer,
    done: &mut Outcome,
    target: &LinkTarget,
    agent: &'static AgentDef,
    on: bool,
) -> Result<(), InstallError> {
    if on {
        done.changes
            .extend(installer.link(&target.name, &target.origin, agent)?.changes);
        return Ok(());
    }
    // A link parked in the agent's disabled directory is not where `unlink`
    // looks, so there is nothing to unlink until it is moved back.
    if target.parked {
        done.changes.extend(
            installer
                .enable(&target.name, &target.origin, agent)?
                .changes,
        );
    }
    done.changes
        .extend(installer.unlink(&target.name, agent)?.changes);
    Ok(())
}

/// What the list says when it has no rows: what is not here, and what to do
/// about it.
///
/// `action` is the step forward, when there is one the sentence cannot take on
/// its own. It sits apart from the two lines of text because it is a separate
/// group, not a third line.
fn empty_state(
    title: &'static str,
    detail: SharedString,
    action: Option<Button>,
    cx: &mut Context<Skillbase>,
) -> AnyElement {
    v_flex()
        .py_8()
        // The spine the row text sits on: the scroll container has already
        // inset by 8, and a row adds 12.
        .px_3()
        .gap_3()
        .child(
            v_flex()
                .gap_1()
                // Weight, not colour alone, is what makes the first line the
                // title of the second.
                .child(div().text_sm().font_medium().child(title))
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(detail),
                ),
        )
        // In a row of its own so the button takes its own width rather than
        // the column's.
        .children(action.map(|action| h_flex().child(action)))
        .into_any_element()
}

/// What the band's title means, in the words the filter behind it actually
/// tests.
///
/// Used twice: as a tooltip on the title, so the vocabulary is explained where
/// the word is written rather than behind a button at the far end of the band,
/// and as the body of the empty state, where the rule for landing in a group is
/// the one thing a reader of an empty group wants.
fn scope_explanation(scope: Scope) -> SharedString {
    match scope {
        Scope::Library(library) => library.explanation().into(),
        Scope::Agent(_) => format!(
            "{} sees a skill when it has a link of its own, or when it reads ~/.agents/skills \
             and the skill is there.",
            scope.title()
        )
        .into(),
    }
}

/// The number beside the band's title: the rows on screen, and out of how many
/// when a search is hiding some.
///
/// One number when nothing is hidden, because "2 of 2" is a comparison with
/// nothing on the other side of it.
fn count_label(shown: usize, total: usize) -> SharedString {
    if shown == total {
        total.to_string().into()
    } else {
        format!("{shown} of {total}").into()
    }
}

/// A tooltip carrying a sentence, capped at a width it can be read at.
///
/// Every text tooltip in this column is built here. `Tooltip::new` lays its
/// text out on a single line however long the text is, so a description of
/// several sentences drew a box wider than the window — worse than the
/// truncated row it was there to explain.
///
/// The cap has to sit on an element of our own rather than on the tooltip: the
/// tooltip's box is a row that takes whatever width its content asks for, and
/// it is the block the text is laid out in that decides where the lines break.
/// A block with a width also breaks a run that has no spaces in it, so a
/// description written as one long word wraps rather than spilling out.
fn text_tooltip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView {
    let text = text.into();
    move |window, cx| {
        let text = text.clone();
        Tooltip::element(move |_, _| div().max_w(px(TOOLTIP_WIDTH)).child(text.clone()))
            .build(window, cx)
    }
}

/// The whole of a row's description, when the row is too narrow to have shown
/// it.
///
/// The column cannot be measured from here, so this is a length: at the 320pt
/// default the description shares its line with the agent marks and truncates
/// somewhere around here. It is deliberately generous — a tooltip that repeats
/// a line the reader can already see is worse than one that is occasionally
/// missing.
fn long_enough_to_truncate(description: &SharedString) -> Option<SharedString> {
    /// Characters that fit on the description line before it truncates.
    const FITS: usize = 34;

    (description.chars().count() > FITS).then(|| description.clone())
}

/// Why this name cannot be used, or `None` when it can.
///
/// An empty field is not a mistake yet, so it has no message: the line under
/// the field already says what shape a name takes, and Create being off is
/// what says the field is not finished.
///
/// `scan` is the last look at the machine, or `None` before one has landed. A
/// name that is already a skill somewhere is refused here rather than by the
/// installer several hundred milliseconds later, and a name that would land as
/// a second copy of one is refused too — a duplicate is what the Duplicates
/// group exists to report, and creating one on purpose is not a step forward.
fn name_problem(name: &str, scan: Option<&Scan>) -> Option<SharedString> {
    if name.is_empty() {
        return None;
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Some(format!("Too long. A name is at most {MAX_NAME_LEN} characters.").into());
    }
    if !is_kebab_case(name) {
        return Some(
            "Use lowercase letters, digits, and single hyphens between them: my-new-skill.".into(),
        );
    }
    if scan.is_some_and(|scan| scan.get(name).is_some()) {
        return Some(format!("“{name}” is already a skill on this machine.").into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use skillbase_core::{Location, LocationKind};
    use std::path::PathBuf;

    fn skill_named(name: &str, description: &str) -> SkillView {
        SkillView {
            name: name.to_string().into(),
            description: description.to_string().into(),
            origin: PathBuf::from("/store").join(name),
            managed: true,
            parse_error: None,
            issues: Vec::new(),
            conflicts: Vec::new(),
            locations: Vec::new(),
            codex_disabled: false,
            visible_to: Vec::new(),
            in_shared: false,
            provenance: None,
        }
    }

    /// A scan holding these skills and nothing else.
    ///
    /// `Scan::load` counts each Library row as it maps the disk; a scan built
    /// by hand has to fill the one row these tests ask for. `All` is the first
    /// of [`Library::ALL`].
    fn scan_of(skills: Vec<SkillView>) -> Scan {
        let mut library_counts = [0usize; Library::ALL.len()];
        library_counts[0] = skills.len();
        Scan {
            skills,
            library_counts,
            ..Scan::default()
        }
    }

    fn scan_holding(name: &str) -> Scan {
        scan_of(vec![skill_named(name, "")])
    }

    #[test]
    fn an_empty_name_is_not_yet_a_mistake() {
        // Nothing to complain about before anything has been typed: the line
        // under the field is the help text, and Create is off because the
        // caller checks for empty, not because of a message.
        assert_eq!(name_problem("", None), None);
    }

    #[test]
    fn a_name_that_is_not_kebab_case_says_so_before_it_is_submitted() {
        for name in [
            "My Skill",
            "my_skill",
            "-lead",
            "trail-",
            "double--hyphen",
            "Ünicode",
        ] {
            let said =
                name_problem(name, None).unwrap_or_else(|| panic!("{name} should be refused"));
            assert!(said.contains("my-new-skill"), "{name}: {said}");
        }
        assert_eq!(name_problem("my-new-skill", None), None);
        assert_eq!(name_problem("pdf2", None), None);
    }

    #[test]
    fn a_name_longer_than_the_limit_is_refused_by_length_and_not_by_shape() {
        let long = "a".repeat(MAX_NAME_LEN + 1);
        let said = name_problem(&long, None).expect("too long");
        assert!(said.contains("Too long"), "{said}");
        assert_eq!(name_problem(&"a".repeat(MAX_NAME_LEN), None), None);
    }

    #[test]
    fn a_name_already_on_the_machine_is_refused_before_the_installer_sees_it() {
        let scan = scan_holding("pdf");
        let said = name_problem("pdf", Some(&scan)).expect("already exists");
        assert!(said.contains("already a skill"), "{said}");
        assert_eq!(name_problem("pdf-two", Some(&scan)), None);
    }

    /// The band used to report the scope's whole count while the search below
    /// it had cut the list to one row, so the number on screen disagreed with
    /// the rows under it.
    #[test]
    fn the_count_in_the_band_is_the_number_of_rows_on_screen() {
        let scan = scan_of(vec![
            skill_named(
                "docx",
                "Use this skill whenever the user works with Word documents.",
            ),
            skill_named(
                "pdf",
                "Use this skill whenever the user works with PDF files.",
            ),
        ]);
        let scope = Scope::Library(Library::All);
        let total = scan.count(scope, |_| false);
        assert_eq!(total, 2);

        let rows =
            |query: &str| listed(&scan, scope, query, SkillSort::Name, &|_| false, &|_| 0).len();

        // Nothing hidden: one number, because "2 of 2" compares a thing with
        // itself.
        assert_eq!(count_label(rows(""), total), "2");
        // A search that hides one row says which of the two are showing.
        assert_eq!(rows("pdf"), 1);
        assert_eq!(count_label(rows("pdf"), total), "1 of 2");
        // A search matching a description, not a name, counts the same way.
        assert_eq!(count_label(rows("Word"), total), "1 of 2");
        assert_eq!(count_label(rows("no-such-thing"), total), "0 of 2");
    }

    /// `pdf` and `docx` both open "Use this skill whenever the…", so the line
    /// on screen is the same for both and the rest of the sentence is the only
    /// thing that tells them apart.
    #[test]
    fn a_description_too_long_for_the_row_is_offered_in_full() {
        let long: SharedString = "Use this skill whenever the user works with PDF files.".into();
        assert_eq!(long_enough_to_truncate(&long), Some(long.clone()));
        assert_eq!(long_enough_to_truncate(&"Short enough.".into()), None);
    }

    #[test]
    fn every_scope_can_say_what_it_holds() {
        for library in Library::ALL {
            let said = scope_explanation(Scope::Library(library));
            assert!(!said.is_empty(), "{library:?} has nothing to say");
        }
        let said = scope_explanation(Scope::Agent("claude-code"));
        assert!(said.contains("Claude Code"), "{said}");
    }

    /// The row icons used to be drawn from `visible_to`, which names every
    /// agent that reads ~/.agents/skills whether or not it is on the machine —
    /// so a shared skill wore a dozen logos beside a sidebar saying one agent
    /// was here.
    #[test]
    fn a_row_names_only_the_agents_that_are_on_this_machine() {
        let cursor = Registry::get("cursor").expect("cursor is in the registry");

        let mut shared = skill_named("shared-one", "");
        shared.in_shared = true;
        shared.visible_to = vec!["cursor", "gemini-cli", "zed"];
        let named: Vec<&str> = shared
            .reach(&[cursor])
            .into_iter()
            .map(|agent| agent.id)
            .collect();
        assert_eq!(named, ["cursor"]);
        assert!(shared.reach(&[]).is_empty());
        // Presence is the whole of the filter: with all three on the machine
        // all three are named.
        let gemini = Registry::get("gemini-cli").expect("gemini-cli is in the registry");
        let zed = Registry::get("zed").expect("zed is in the registry");
        assert_eq!(shared.reach(&[cursor, gemini, zed]).len(), 3);

        // A link on disk is kept whatever presence says: it is the thing the
        // reader can act on.
        let mut linked = skill_named("linked-one", "");
        linked.visible_to = vec!["claude-code"];
        linked.locations = vec![Location {
            agent_id: "claude-code",
            path: PathBuf::from("/home/.claude/skills/linked-one"),
            kind: LocationKind::Symlink {
                target: PathBuf::from("/store/linked-one"),
            },
        }];
        let named: Vec<&str> = linked
            .reach(&[])
            .into_iter()
            .map(|agent| agent.id)
            .collect();
        assert_eq!(named, ["claude-code"]);
    }
}

/// The window a dialog test opens, and the throwaway home it runs against.
///
/// `window.open_dialog` only stores the builder it is given. The builder runs
/// later, from `Root::render_dialog_layer`, which `Skillbase::render` calls —
/// so the view is borrowed while the builder runs, and a builder that reads
/// the view aborts the process on the frame after the click rather than at the
/// click. Opening a dialog therefore proves nothing on its own: a test has to
/// draw as well, which is what [`drawn`] does.
///
/// This lives here rather than beside either set of tests because the dialogs
/// it is used on are spread across `list` and `discover`, and neither module
/// can see the other's private methods.
#[cfg(test)]
pub(crate) mod dialog_probe {
    use std::fs;
    use std::sync::Once;

    use gpui_kit::component::Root;
    use gpui_kit::{AppContext as _, Entity, TestAppContext, VisualTestContext};

    use crate::app::Skillbase;
    use crate::ui::model::HOME_OVERRIDE_ENV;

    /// Two skills, because the dialogs that act on a marked set need at least
    /// two rows to mark.
    pub(crate) const FIXTURE: [&str; 2] = ["alpha", "beta"];

    static FIXTURE_HOME: Once = Once::new();

    /// A home under the temporary directory, with two skills written into it.
    ///
    /// `SKILLBASE_HOME` points the whole application at it, so nothing in
    /// these tests can reach the real store.
    fn fixture_home() {
        // Per-process, because two test binaries run at once often enough to
        // matter: `cargo test --workspace` starts one while another is still
        // going, and a shared path let one write the fixture while the other
        // scanned it. That raced, and the tests that read the scan failed
        // about once in twenty runs.
        let home =
            std::env::temp_dir().join(format!("skillbase-dialog-probe-{}", std::process::id()));
        FIXTURE_HOME.call_once(|| {
            // A recycled pid must not inherit the last run's store.
            let _ = fs::remove_dir_all(&home);
            for name in FIXTURE {
                let dir = home.join(".agents/skills").join(name);
                fs::create_dir_all(&dir).expect("a fixture directory");
                fs::write(
                    dir.join("SKILL.md"),
                    format!(
                        "---\nname: {name}\ndescription: A skill for the dialog tests to \
                         act on.\n---\n\nNothing here is run.\n"
                    ),
                )
                .expect("a fixture SKILL.md");
            }
            // Sound because every test that reads this variable goes through
            // this `Once` first, and nothing in the binary writes it again.
            unsafe { std::env::set_var(HOME_OVERRIDE_ENV, &home) };
        });
    }

    /// A drawn window holding a real `Skillbase` that has scanned the fixture.
    pub(crate) fn window(cx: &mut TestAppContext) -> (VisualTestContext, Entity<Skillbase>) {
        fixture_home();
        cx.update(gpui_kit::init);

        let handle = cx.add_window(|window, cx| {
            let view = cx.new(|cx| Skillbase::new(window, cx));
            Root::new(view, window, cx)
        });
        cx.run_until_parked();

        let mut cx = VisualTestContext::from_window(handle.into(), cx);
        let skillbase = cx.update(|window, cx| {
            window
                .root::<Root>()
                .flatten()
                .expect("the window has a Root")
                .read(cx)
                .view()
                .clone()
                .downcast::<Skillbase>()
                .expect("the Root holds the Skillbase view")
        });

        let scanned = cx.update(|_, cx| skillbase.read(cx).scan().is_some());
        assert!(scanned, "the fixture home was never scanned");

        (cx, skillbase)
    }

    /// Draw the window, and fail if no dialog reached the screen.
    ///
    /// The draw is the assertion: it is the only thing that runs a dialog
    /// builder.
    pub(crate) fn drawn(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("dialog-layer").is_some(),
            "the dialog was opened but never drew"
        );
    }

    /// Draw the window, and fail if the skill list did not reach the screen.
    ///
    /// The list's own states — the rows, the band's menu, the marked band and
    /// each empty state — are built during the draw and nowhere else, so this
    /// is the only thing that runs them.
    pub(crate) fn list_drawn(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("skill-list").is_some(),
            "the skill list never drew"
        );
    }
}

#[cfg(test)]
mod dialog_tests {
    use gpui_kit::TestAppContext;

    use super::dialog_probe::{FIXTURE, drawn, window};
    use super::name_problem;

    #[gpui_kit::test]
    fn the_new_skill_dialog_opens(cx: &mut TestAppContext) {
        let (mut cx, skillbase) = window(cx);
        cx.update(|window, cx| {
            skillbase.update(cx, |this, cx| this.open_new_skill_dialog(window, cx));
        });
        drawn(&mut cx);
    }

    /// The line under the Name field is written from the scan the dialog took
    /// when it opened, so the scan has to be the one that found the fixture.
    #[gpui_kit::test]
    fn the_new_skill_dialog_still_knows_which_names_are_taken(cx: &mut TestAppContext) {
        let (mut cx, skillbase) = window(cx);
        cx.update(|window, cx| {
            skillbase.update(cx, |this, cx| this.open_new_skill_dialog(window, cx));
        });
        drawn(&mut cx);

        let scan = cx.update(|_, cx| skillbase.read(cx).scan().cloned());
        let taken = name_problem(FIXTURE[0], scan.as_deref()).expect("the fixture name is taken");
        assert!(taken.contains("already a skill"), "{taken}");
        assert_eq!(name_problem("not-installed-yet", scan.as_deref()), None);
    }

    /// Typing redraws the dialog, which runs its builder again. The rule under
    /// the field is written on that pass, so this is the path that has to hold
    /// up frame after frame.
    #[gpui_kit::test]
    fn the_new_skill_dialog_survives_typing_into_it(cx: &mut TestAppContext) {
        let (mut cx, skillbase) = window(cx);
        cx.update(|window, cx| {
            skillbase.update(cx, |this, cx| this.open_new_skill_dialog(window, cx));
        });
        drawn(&mut cx);

        let name = cx.update(|_, cx| skillbase.read(cx).new_name.clone());
        for typed in ["My Skill", FIXTURE[0], "my-new-skill"] {
            cx.update(|window, cx| {
                name.update(cx, |state, cx| state.set_value(typed, window, cx));
            });
            drawn(&mut cx);
        }
    }

    #[gpui_kit::test]
    fn the_library_help_dialog_opens(cx: &mut TestAppContext) {
        let (mut cx, skillbase) = window(cx);
        cx.update(|window, cx| {
            skillbase.update(cx, |this, cx| this.open_library_help(window, cx));
        });
        drawn(&mut cx);
    }

    #[gpui_kit::test]
    fn the_link_and_unlink_dialogs_open_for_a_marked_set(cx: &mut TestAppContext) {
        for on in [true, false] {
            let (mut cx, skillbase) = window(cx);
            cx.update(|window, cx| {
                skillbase.update(cx, |this, cx| {
                    this.mark_all(cx);
                    this.open_link_marked_dialog(on, window, cx);
                });
            });
            drawn(&mut cx);
        }
    }

    #[gpui_kit::test]
    fn the_bulk_delete_dialog_opens_for_a_marked_set(cx: &mut TestAppContext) {
        let (mut cx, skillbase) = window(cx);
        cx.update(|window, cx| {
            skillbase.update(cx, |this, cx| {
                this.mark_all(cx);
                this.confirm_delete_marked(window, cx);
            });
        });
        drawn(&mut cx);
    }
}

/// The list's own render paths, each of which is built during a draw and
/// nowhere else.
///
/// They share the dialog probe's window, because a real `Skillbase` over a
/// throwaway home is what the states are made of: a scan with rows in it, a
/// search that matches none of them, and a marked set.
#[cfg(test)]
mod render_tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui_kit::base::ElementExt as _;
    use gpui_kit::{
        AvailableSpace, ParentElement as _, Pixels, Size, TestAppContext, VisualTestContext, div,
        point, px,
    };

    use super::dialog_probe::{list_drawn, window};
    use super::{TOOLTIP_WIDTH, text_tooltip};
    use crate::ui::model::{Library, Scope};

    /// The margin, padding and border the tooltip draws around its text. The
    /// cap is on the text, so the box is that much wider than the cap.
    const CHROME: Pixels = px(48.);

    #[gpui_kit::test]
    fn the_list_draws_its_rows(cx: &mut TestAppContext) {
        let (mut cx, _) = window(cx);
        list_drawn(&mut cx);
    }

    /// The band's menu and every row's context menu are built here. A builder
    /// that read the view while it was being updated would abort on this draw,
    /// which is the failure the dialog probe exists to catch.
    #[gpui_kit::test]
    fn a_marked_set_draws_its_band_and_its_menus(cx: &mut TestAppContext) {
        let (mut cx, skillbase) = window(cx);
        cx.update(|_, cx| {
            skillbase.update(cx, |this, cx| this.mark_all(cx));
        });
        list_drawn(&mut cx);
    }

    /// Two of the three empty states: a search that matched nothing, which
    /// carries the registry button, and a group with nothing in it, which
    /// carries the rule for landing in it. The third is first launch, which
    /// needs a home with no skills at all.
    #[gpui_kit::test]
    fn the_empty_states_draw(cx: &mut TestAppContext) {
        let (mut cx, skillbase) = window(cx);

        let search = cx.update(|_, cx| skillbase.read(cx).search.clone());
        cx.update(|window, cx| {
            search.update(cx, |state, cx| {
                state.set_value("no-skill-is-called-this", window, cx)
            });
        });
        list_drawn(&mut cx);

        cx.update(|window, cx| {
            search.update(cx, |state, cx| state.set_value("", window, cx));
            skillbase.update(cx, |this, cx| {
                // Nothing in the fixture home is a duplicate, so this group is
                // empty on a machine that does hold skills. Set rather than
                // selected: `select_scope` saves the settings file, and every
                // test in this binary shares one home, so the next one to start
                // would open on a scope it did not ask for.
                this.scope = Scope::Library(Library::Conflicts);
                cx.notify();
            });
        });
        list_drawn(&mut cx);
    }

    /// How large a tooltip built by [`text_tooltip`] draws.
    ///
    /// The window lays a tooltip out against its minimum size, which is what
    /// leaves the box free to be as wide as its one line of text — so the
    /// measurement has to be taken the same way for it to say anything.
    fn tooltip_size(cx: &mut VisualTestContext, text: &str) -> Size<Pixels> {
        let measured = Rc::new(Cell::new(Size::default()));
        let build = text_tooltip(text.to_string());
        let out = measured.clone();
        cx.draw(
            point(px(0.), px(0.)),
            AvailableSpace::min_size(),
            move |window, cx| {
                let tooltip = build(window, cx);
                div()
                    .on_prepaint(move |bounds, _, _| out.set(bounds.size))
                    .child(tooltip)
            },
        );
        measured.get()
    }

    /// A tooltip used to be laid out on one line however long its text was, so
    /// the full description offered on a truncated row arrived as a line wider
    /// than the window — worse than the row it was explaining.
    #[gpui_kit::test]
    fn a_tooltip_holding_a_sentence_wraps_inside_a_reading_width(cx: &mut TestAppContext) {
        let (mut cx, _) = window(cx);

        let label = tooltip_size(&mut cx, "Short.");
        // A short label still hugs its text: the cap is a maximum, not a width.
        assert!(
            label.width < px(TOOLTIP_WIDTH),
            "a one-word tooltip drew {:?} wide",
            label.width
        );

        let sentence = tooltip_size(
            &mut cx,
            "Use this skill whenever the user works with PDF files.",
        );
        let paragraph = tooltip_size(
            &mut cx,
            "Use this skill whenever the user works with PDF files: reading one, filling in a \
             form, splitting one apart, or putting several together. It reads the pages and \
             does not change them.",
        );
        // Nothing in this one is a place to break a line, so the break has to
        // fall mid-word. Unwrapped, it was the description that ran off the
        // screen.
        let unbroken = tooltip_size(&mut cx, &"unbrokenrun".repeat(20));

        for (what, size) in [
            ("a sentence", sentence),
            ("a paragraph", paragraph),
            ("one long word", unbroken),
        ] {
            assert!(
                size.width <= px(TOOLTIP_WIDTH) + CHROME,
                "{what} drew {:?} wide, past the {TOOLTIP_WIDTH}pt cap",
                size.width
            );
            assert!(
                size.height > label.height,
                "{what} is wider than the cap, so it has to take more than the one line \
                 a label takes: {:?} against {:?}",
                size.height,
                label.height
            );
        }

        // The lines pile up rather than the box being cut off at some height:
        // three sentences take more of them than one.
        assert!(
            paragraph.height > sentence.height,
            "a paragraph drew {:?} against a sentence's {:?}",
            paragraph.height,
            sentence.height
        );
    }
}
