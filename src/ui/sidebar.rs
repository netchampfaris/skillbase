//! The scope sidebar: a Library group, an Agents group listing only the agents
//! that exist on this machine, and Settings pinned to the bottom.

use gpui_kit::component::sidebar::{
    Sidebar, SidebarCollapsible, SidebarItem, SidebarMenu, SidebarMenuItem,
};
use gpui_kit::component::{ActiveTheme as _, Collapsible, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Context, Div, ElementId, IntoElement, ParentElement as _, SharedString, Styled as _,
    Window, div, px,
};
use skillbase_core::{AgentDef, Registry};

use super::agent_icon;

use crate::app::Skillbase;

use super::model::{Library, Scope};

/// Wide enough for the longest scope label, and visibly subordinate to the
/// work area.
pub const SIDEBAR_WIDTH: f32 = 240.;

impl Skillbase {
    pub(crate) fn render_sidebar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let scope = self.scope;
        let collapsed = self.sidebar_collapsed;
        let in_settings = self.showing_settings;
        let scan = self.scan();

        let library: Vec<_> =
            Library::ALL
                .into_iter()
                .map(|library| {
                    let target = Scope::Library(library);
                    let total = scan.map(|scan| scan.count(target));
                    SidebarMenuItem::new(library.label())
                        .icon(library_icon(library))
                        .active(!in_settings && scope == target)
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
                    let total = scan.map(|scan| scan.count(target));
                    // An agent with no directory is shown in the muted weight the
                    // counts use, so the row does not claim the agent is here.
                    let absent = scan.is_some() && !installed.contains(&agent);
                    SidebarMenuItem::new(agent.display_name)
                        .icon(agent_icon(agent))
                        .active(!in_settings && scope == target)
                        .suffix(move |_, cx| {
                            if absent {
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("not installed")
                            } else {
                                count_label(total, cx)
                            }
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.select_scope(target, window, cx)
                        }))
                })
                .collect();

        // Settings goes into the footer as a bare menu rather than wrapped in
        // `SidebarFooter`. That wrapper adds its own padding on top of the
        // footer region's, which indents the row 8px past every row above it,
        // and paints a second hover background around the item's own. Once the
        // sidebar collapses to 48px the doubled padding leaves the item no
        // width at all and clips the icon out of sight.
        let settings = scope_menu()
            .child(
                SidebarMenuItem::new("Settings")
                    .icon(IconName::Settings)
                    .active(in_settings)
                    .on_click(cx.listener(|this, _, _, cx| this.show_settings(cx))),
            )
            .collapsed(collapsed)
            .render("settings", window, cx);

        Sidebar::new("scopes")
            .collapsible(SidebarCollapsible::Icon)
            .collapsed(collapsed)
            .w(px(SIDEBAR_WIDTH))
            .child(ScopeGroup::first("Library", library))
            .child(ScopeGroup::new("Agents", agents))
            .footer(settings)
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
/// not `Styled`, so a caller cannot add the space. It also drops its label
/// when the sidebar collapses, leaving two runs of icons with no boundary
/// between them at all.
#[derive(Clone)]
struct ScopeGroup {
    label: SharedString,
    items: Vec<SidebarMenuItem>,
    /// True for the group at the top, which needs no space or rule above it.
    leading: bool,
    collapsed: bool,
}

impl ScopeGroup {
    fn new(label: impl Into<SharedString>, items: Vec<SidebarMenuItem>) -> Self {
        Self {
            label: label.into(),
            items,
            leading: false,
            collapsed: false,
        }
    }

    fn first(label: impl Into<SharedString>, items: Vec<SidebarMenuItem>) -> Self {
        Self {
            leading: true,
            ..Self::new(label, items)
        }
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
        let leading = self.leading;

        div()
            .flex()
            .flex_col()
            // Sections stand apart; rows within a section do not.
            .when(!leading, |this| this.pt_4())
            .when(!collapsed, |this| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .flex_shrink_0()
                        .h_7()
                        .px_2()
                        .text_xs()
                        .text_color(cx.theme().sidebar_foreground.opacity(0.7))
                        .child(self.label.clone()),
                )
            })
            .when(collapsed && !leading, |this| {
                // With no room for the label, a hairline says the same thing:
                // these two runs of icons are different kinds of scope.
                this.child(
                    div()
                        .flex_shrink_0()
                        .mx_2()
                        .mb_4()
                        .h(px(1.))
                        .bg(cx.theme().border),
                )
            })
            .child(
                scope_menu()
                    .children(self.items)
                    .collapsed(collapsed)
                    .render(id, window, cx),
            )
    }
}

fn library_icon(library: Library) -> IconName {
    match library {
        Library::All => IconName::LayoutDashboard,
        Library::Shared => IconName::Globe,
        Library::Managed => IconName::CircleCheck,
        Library::Unmanaged => IconName::Folder,
        Library::Invalid => IconName::TriangleAlert,
        Library::Conflicts => IconName::Copy,
    }
}

/// The count trailing a scope row. Neutral: it is metadata, not a state. A
/// dash stands in until the first scan lands, so the row does not claim zero.
fn count_label(total: Option<usize>, cx: &App) -> Div {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(match total {
            Some(total) => total.to_string(),
            None => "—".to_string(),
        })
}
