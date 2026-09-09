//! The sidebar column: its two header bands, a top group of Discover and the
//! three commands that put a skill in the store, a Library group, a collapsible
//! Agents group listing only the agents that exist on this machine, and
//! Settings pinned to the bottom.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::sidebar::{Sidebar, SidebarItem, SidebarMenu, SidebarMenuItem};
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Collapsible, Icon, IconName, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, ClickEvent, Context, ElementId, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    px, rems,
};
use skillbase_core::{AgentDef, Registry};

use crate::app::{Skillbase, UpdateState, WorkArea};

use super::agent_icon;
use super::model::{Library, Scope};
use super::{BAND_HEIGHT, TRAFFIC_LIGHT_INSET, drag_band};

/// Wide enough for the longest scope label, and visibly subordinate to the
/// work area.
pub const SIDEBAR_WIDTH: f32 = 240.;

impl Skillbase {
    /// True once an update check has landed, whatever it found.
    ///
    /// Everything that counts skills behind upstream has to wait for this: the
    /// answer comes over the network long after the disk has been read, and
    /// before it arrives there is no number, not a zero.
    pub(crate) fn checked_for_updates(&self) -> bool {
        matches!(self.updates, UpdateState::Ready(_))
    }

    /// True while the check is in flight, which is a different sentence from
    /// never having run one.
    pub(crate) fn checking_for_updates(&self) -> bool {
        matches!(self.updates, UpdateState::Checking)
    }

    pub(crate) fn render_sidebar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let scope = self.scope;
        let in_library = self.work_area == WorkArea::Skills;
        let scan = self.scan();

        // Discover is a destination; the other three are commands that land a
        // skill in the same store. They share a group, headed "Add skills",
        // because they are how a skill that is not here yet gets here.
        let discover = SidebarMenuItem::new("Discover")
            .icon(IconName::Search)
            .active(self.work_area == WorkArea::Discover)
            .on_click(cx.listener(|this, _, window, cx| this.show_discover(window, cx)));

        let new_skill = SidebarMenuItem::new("New skill")
            .icon(IconName::Plus)
            .on_click(cx.listener(|this, _, window, cx| this.open_new_skill_dialog(window, cx)));

        let install = SidebarMenuItem::new("Install from GitHub")
            .icon(IconName::Github)
            .on_click(cx.listener(|this, _, window, cx| this.open_install_dialog(window, cx)));

        // Beside the GitHub row, because it answers the same question with a
        // different source: a skill that is already on this machine — sent
        // over, or sitting in a repository that is already cloned — and has no
        // way into the store without it.
        let import = SidebarMenuItem::new("Install from folder")
            .icon(IconName::FolderOpen)
            .on_click(cx.listener(|this, _, window, cx| this.import_from_folder(window, cx)));

        let library: Vec<_> =
            Library::ALL
                .into_iter()
                .map(|library| {
                    let target = Scope::Library(library);
                    // The Updates row is the one whose number nothing on this
                    // machine can answer. Until the check lands it wears the
                    // same skeleton a row wears before the scan does, because
                    // a zero here would read as "nothing is behind upstream"
                    // when what happened is that nobody has asked.
                    let total = if library == Library::Updates && !self.checked_for_updates() {
                        None
                    } else {
                        scan.map(|scan| scan.count(target, |name| self.has_update(name)))
                    };
                    SidebarMenuItem::new(library.label())
                        .icon(library_icon(library))
                        .active(in_library && scope == target)
                        .suffix(move |_, cx| count_label(total, cx))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.select_scope(target, window, cx)
                        }))
                })
                .collect();

        // Only agents whose global directory exists, unless Settings says
        // otherwise. An agent that is not installed is hidden rather than
        // greyed, per SPEC §5.1; the "Show all agents" preference reveals the
        // rest, and each one still says how many skills it would see.
        let installed = scan.map(|scan| scan.installed.clone()).unwrap_or_default();
        let listed: Vec<&'static AgentDef> = if self.preferences.show_all_agents {
            Registry::all().iter().filter(|a| !a.is_shared()).collect()
        } else {
            installed.clone()
        };
        let agents: Vec<_> =
            listed
                .into_iter()
                .map(|agent| {
                    let target = Scope::Agent(agent.id);
                    let total = scan.map(|scan| scan.count(target, |name| self.has_update(name)));
                    // An agent with no directory is shown in the muted weight the
                    // counts use, so the row does not claim the agent is here.
                    let absent = scan.is_some() && !installed.contains(&agent);
                    SidebarMenuItem::new(agent.display_name)
                        .icon(agent_icon(agent))
                        .active(in_library && scope == target)
                        .suffix(move |_, cx| {
                            if absent {
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("not installed")
                                    .into_any_element()
                            } else {
                                count_label(total, cx)
                            }
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.select_scope(target, window, cx)
                        }))
                })
                .collect();

        let settings_item = SidebarMenuItem::new("Settings")
            .icon(IconName::Settings)
            .active(self.work_area == WorkArea::Settings)
            .on_click(cx.listener(|this, _, _, cx| this.show(WorkArea::Settings, cx)));

        v_flex()
            .h_full()
            .flex_shrink_0()
            .w(px(SIDEBAR_WIDTH))
            // The step from this background to the work area's is the boundary
            // — one edge down the full height of the column, so the two bands
            // and the navigation under them cannot draw it at different widths.
            // There is no hairline: `sidebar.border` is fully transparent in
            // both themes, so the `border_r_1` that used to sit here only spent
            // a pixel of layout width painting nothing.
            .bg(cx.theme().tokens.sidebar)
            .child(self.sidebar_identity_band(window, cx))
            .child(self.sidebar_name_band(cx))
            .child(
                div().flex().flex_1().min_h_0().child(
                    Sidebar::new("scopes")
                        .w_full()
                        .border_r_0()
                        .child(
                            ScopeGroup::new(
                                "Add skills",
                                vec![discover, new_skill, install, import],
                            )
                            .leading(),
                        )
                        .child(ScopeGroup::new("Library", library))
                        .child(
                            ScopeGroup::new("Agents", agents)
                                // The group starts closed and lists only the
                                // agents that are here, so without this the
                                // reader cannot tell whether Skillbase knows
                                // about the agent they are missing. It says
                                // "installed" because that is what the two
                                // numbers are about: with "Show all agents"
                                // on, every agent has a row and the count
                                // still reports how many are on the machine.
                                .count(scan.map(|scan| {
                                    SharedString::from(format!(
                                        "{} of {} installed",
                                        scan.installed.len(),
                                        Registry::all().iter().filter(|a| !a.is_shared()).count()
                                    ))
                                }))
                                .folded(self.agents_collapsed)
                                .on_toggle(Rc::new(cx.listener(|this, _, window, cx| {
                                    this.agents_collapsed = !this.agents_collapsed;
                                    this.remember_agents_group(window, cx);
                                    cx.notify();
                                }))),
                        )
                        // Settings goes into the footer as a bare menu rather
                        // than wrapped in `SidebarFooter`. That wrapper adds its
                        // own padding on top of the footer region's, which
                        // indents the row 8px past every row above it, and
                        // paints a second hover background around the item's
                        // own.
                        .footer(
                            scope_menu()
                                .child(settings_item)
                                .render("settings", window, cx),
                        ),
                ),
            )
    }

    /// The sidebar's first band: nothing but the collapse control, because the
    /// macOS traffic lights take the rest of the row.
    ///
    /// A `drag_band` rather than `TitleBar`: the latter is 34px tall before
    /// any override, which sat the toggle off the traffic lights' centre line.
    fn sidebar_identity_band(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        drag_band("sidebar-identity", window, cx)
            .flex_shrink_0()
            .h(BAND_HEIGHT)
            .pl(px(TRAFFIC_LIGHT_INSET))
            .bg(cx.theme().tokens.sidebar)
            .items_center()
            .child(
                // The `Button` `SidebarToggleButton` wraps, rather than the
                // wrapper itself: the wrapper exposes no way to name the
                // control, and this one hides half the interface. The band is
                // rendered only while the sidebar is showing, so the button is
                // always the hide half of the pair.
                Button::new("collapse")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::PanelLeftClose).size_4())
                    .tooltip("Hide sidebar")
                    .accessibility_label("Hide sidebar")
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_sidebar(window, cx))),
            )
    }

    /// The sidebar's second band: what the application is called, and the one
    /// command whose scope is the whole machine rather than the visible list.
    fn sidebar_name_band(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let home = self.roots.home().display().to_string();

        h_flex()
            .flex_shrink_0()
            .h_11()
            .px_3()
            .gap_2()
            .items_center()
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    // The rows below inset their contents by this much again
                    // inside the same padding, and the name sits on that spine.
                    .pl_2()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().font_medium().truncate().child("Skillbase"))
                    .when(self.home_overridden, |this| {
                        // Every mutation lands under this directory, so where
                        // it points is named rather than implied.
                        this.child(
                            div()
                                .id("home-override")
                                .flex_shrink_0()
                                .tooltip(move |window, cx| {
                                    Tooltip::new(format!("SKILLBASE_HOME={home}")).build(window, cx)
                                })
                                .child(
                                    Icon::new(IconName::TriangleAlert)
                                        .xsmall()
                                        .text_color(cx.theme().warning),
                                ),
                        )
                    }),
            )
            .child(
                Button::new("refresh")
                    .ghost()
                    .small()
                    .icon(IconName::RotateCw)
                    // Said here rather than only in Settings: this is the
                    // control someone hesitates over, and what stops them is
                    // not knowing whether a scan writes anything.
                    .tooltip("Re-scan every scope. Scanning only reads; nothing on disk changes")
                    .accessibility_label("Re-scan every scope")
                    // The one gesture that means "look at the machine again",
                    // which is why it is also the one that spends requests on
                    // an update check without being asked twice.
                    .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))),
            )
    }
}

/// A menu of scope rows.
///
/// The framework's default is an 8px gap, which reads as a list of separate
/// controls. These rows are one navigation surface, so they sit 2px apart and
/// the groups do the separating.
fn scope_menu() -> SidebarMenu {
    SidebarMenu::new().gap_0p5()
}

/// One labelled block of scope rows.
///
/// `SidebarGroup` would do this, but it puts nothing above its label, so the
/// first row of a group crowds the last row of the one before it; and it is
/// not `Styled`, so a caller cannot add the space. It also insists on a label,
/// which the run of commands at the top does not want.
#[derive(Clone)]
struct ScopeGroup {
    label: Option<SharedString>,
    /// What the heading says about how much of the group is listed, when the
    /// rows alone cannot say it. `None` for a group that has nothing to
    /// report, and before the scan lands: the label beside it is short enough
    /// that a count arriving late moves nothing.
    count: Option<SharedString>,
    items: Vec<SidebarMenuItem>,
    /// True for the group at the top, which needs no space above it.
    leading: bool,
    /// The sidebar rail asked this group to fold to an icon. Distinct from
    /// [`ScopeGroup::folded`], which is the Agents heading the user clicks.
    collapsed: bool,
    /// True when the group's own rows are hidden behind its heading. Has no
    /// effect while the rail is also collapsed, since collapsing hides the
    /// heading and leaves nothing to unfold with.
    folded: bool,
    on_toggle: Option<Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>>,
}

impl ScopeGroup {
    fn new(label: impl Into<SharedString>, items: Vec<SidebarMenuItem>) -> Self {
        Self {
            label: Some(label.into()),
            count: None,
            items,
            leading: false,
            collapsed: false,
            folded: false,
            on_toggle: None,
        }
    }

    /// Say in the heading how much of the group is listed.
    fn count(mut self, count: Option<SharedString>) -> Self {
        self.count = count;
        self
    }

    /// Mark the group as the first one, which needs no space above it: the
    /// list already pads its first item.
    fn leading(mut self) -> Self {
        self.leading = true;
        self
    }

    /// Hide the rows, leaving the heading as the control that brings them
    /// back. Ignored while the rail is collapsed, for the same reason.
    fn folded(mut self, folded: bool) -> Self {
        self.folded = folded;
        self
    }

    fn on_toggle(mut self, on_toggle: Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>) -> Self {
        self.on_toggle = Some(on_toggle);
        self
    }
}

impl Collapsible for ScopeGroup {
    fn is_collapsed(&self) -> bool {
        self.collapsed
    }

    fn collapsed(mut self, collapsed: bool) -> Self {
        self.collapsed = collapsed;
        self
    }
}

impl SidebarItem for ScopeGroup {
    fn render(
        self,
        id: impl Into<ElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        let id = id.into();
        let collapsed = self.collapsed;
        let folded = self.folded;
        let leading = self.leading;
        let toggle = self.on_toggle.clone();
        // The rail collapse hides the heading; folding the section does not.
        let label = self.label.clone().filter(|_| !collapsed);
        let count = self.count.clone().filter(|_| !collapsed);
        // `SidebarMenu::collapsed` does not hide rows, it renders each one
        // icon-only and centred — the look the rail collapse wants, not the
        // look a folded section wants. So a folded section skips the menu
        // entirely instead of passing `folded` through as `collapsed`. This
        // only applies while the rail is expanded: collapsed, there is no
        // heading to unfold with, so the rows stay icon-only regardless of
        // `folded`.
        let hide_rows = folded && !collapsed;

        div()
            .flex()
            .flex_col()
            // Sections stand apart; rows within a section do not.
            .when(!leading, |this| this.pt_4())
            .when_some(label, |this, label| {
                this.child(section_heading(label, count, folded, toggle, cx))
            })
            .when(!hide_rows, |this| {
                this.child(
                    scope_menu()
                        .children(self.items)
                        .collapsed(collapsed)
                        .render(id, window, cx),
                )
            })
    }
}

fn section_heading(
    label: SharedString,
    count: Option<SharedString>,
    folded: bool,
    toggle: Option<Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>>,
    cx: &App,
) -> AnyElement {
    let muted = cx.theme().sidebar_foreground.opacity(0.7);
    // `flex_1` rather than an intrinsic width: it takes the slack a `Button`
    // would otherwise spend centring its content, which pushes the chevron
    // to the trailing edge where a fold control belongs.
    let text = div()
        .flex_1()
        .min_w_0()
        .text_xs()
        .text_color(muted)
        .child(label.clone());
    // Read out together, because the count is part of what the heading says.
    let spoken = match &count {
        Some(count) => SharedString::from(format!("{label}, {count}")),
        None => label.clone(),
    };
    // The same size and colour as the label: this is part of the heading, not
    // a second thing beside it.
    let count = count.map(|count| {
        div()
            .flex_shrink_0()
            .text_xs()
            .text_color(muted)
            .child(count)
    });

    let Some(toggle) = toggle else {
        return h_flex()
            .flex_shrink_0()
            .h_6()
            .px_2()
            .gap_2()
            .items_center()
            .child(text)
            .children(count)
            .into_any_element();
    };

    // A real button rather than a div wearing a hover: this heading is the
    // control that folds the section, so it has to be a tab stop, show that it
    // is being pressed, and announce itself. `small` is what gives a Button
    // the same 24px height, 8px padding and 4px gap the plain heading has;
    // what changes is that the hover is now the one every other ghost control
    // in the sidebar uses.
    Button::new(ElementId::from(format!("scope-heading-{label}")))
        .ghost()
        .small()
        .w_full()
        .accessibility_label(spoken)
        // Expanded, not selected: what the control reports is whether the rows
        // under it are showing.
        .toggled(!folded)
        .child(text)
        .children(count)
        .child(
            Icon::new(if folded {
                IconName::ChevronRight
            } else {
                IconName::ChevronDown
            })
            .xsmall()
            .flex_shrink_0()
            .text_color(muted),
        )
        .on_click(move |event, window, cx| toggle(event, window, cx))
        .into_any_element()
}

fn library_icon(library: Library) -> IconName {
    match library {
        // A stack of sheets, for the group that is every sheet. A dashboard
        // grid said "panels", which is not what this row shows.
        Library::All => IconName::GalleryVerticalEnd,
        // Linked nodes, for the one directory the agents all read. A globe
        // said "the internet"; ~/.agents/skills is on this machine.
        Library::Shared => IconName::Network,
        Library::Managed => IconName::CircleCheck,
        Library::Unmanaged => IconName::Folder,
        // The same glyph the list rows mark an update with, so the row and the
        // group cannot be read as two different facts.
        Library::Updates => IconName::ArrowDown,
        Library::Invalid => IconName::TriangleAlert,
        Library::Conflicts => IconName::Copy,
    }
}

/// The count trailing a scope row. Neutral: it is metadata, not a state. Same
/// size as the row's name, so the two read as one label.
///
/// Until the first scan lands there is no number, and the row wears the same
/// skeleton the list's rows do rather than a dash: a dash reads as a value —
/// zero, or not applicable — which is the wrong thing to say about a count
/// that is still being computed. It is sized to a two-digit count so the row
/// does not reflow when the real one arrives.
fn count_label(total: Option<usize>, cx: &App) -> AnyElement {
    match total {
        Some(total) => div()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(total.to_string())
            .into_any_element(),
        None => Skeleton::new()
            .h(rems(0.8))
            .w(rems(1.25))
            .into_any_element(),
    }
}
