//! The sidebar column: its two header bands, a New skill row, a Library group,
//! an Agents group listing only the agents that exist on this machine, and
//! Settings pinned to the bottom.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::sidebar::{
    Sidebar, SidebarItem, SidebarMenu, SidebarMenuItem, SidebarToggleButton,
};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Collapsible, Icon, IconName, Sizable as _, StyledExt as _, TitleBar, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Context, Div, ElementId, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use skillbase_core::{AgentDef, Registry};

use super::agent_icon;

use crate::app::Skillbase;

use super::BAND_HEIGHT;
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
        let in_settings = self.showing_settings;
        let scan = self.scan();

        // A new skill lands in the store whatever the selected scope is, so it
        // reads as a destination's peer rather than as a list control.
        let new_skill = SidebarMenuItem::new("New skill")
            .icon(IconName::Plus)
            .on_click(cx.listener(|this, _, window, cx| this.open_new_skill_dialog(window, cx)));

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

        let settings_item = SidebarMenuItem::new("Settings")
            .icon(IconName::Settings)
            .active(in_settings)
            .on_click(cx.listener(|this, _, _, cx| this.show_settings(cx)));

        v_flex()
            .h_full()
            .flex_shrink_0()
            .w(px(SIDEBAR_WIDTH))
            .bg(cx.theme().tokens.sidebar)
            // The column owns the boundary with the work area, so the two bands
            // and the navigation under them cannot draw it at different widths.
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .child(self.sidebar_identity_band(cx))
            .child(self.sidebar_name_band(cx))
            .child(
                div().flex().flex_1().min_h_0().child(
                    Sidebar::new("scopes")
                        .w_full()
                        .border_r_0()
                        .child(ScopeGroup::leading(vec![new_skill]))
                        .child(ScopeGroup::new("Library", library))
                        .child(ScopeGroup::new("Agents", agents))
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
    /// `TitleBar` is what stands in here rather than a plain row, for the inset
    /// those lights need and for the window drag and double-click zoom it
    /// already owns. Its bottom hairline is painted in the sidebar's own colour
    /// so the band and the sidebar read as one surface.
    fn sidebar_identity_band(&self, cx: &mut Context<Self>) -> impl IntoElement {
        TitleBar::new()
            .h(BAND_HEIGHT)
            .bg(cx.theme().tokens.sidebar)
            .border_color(cx.theme().tokens.sidebar)
            .child(
                h_flex().h_full().items_center().child(
                    SidebarToggleButton::new()
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx))),
                ),
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
                    .tooltip("Re-scan every scope")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.rescan(None, window, cx);
                        // Cheap after the first read, which is cached, and
                        // Refresh is the one gesture that means "look at the
                        // machine again".
                        this.count_usage(window, cx);
                    })),
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
    /// `None` for the group of commands at the top, which is short enough and
    /// distinct enough that a heading would only name what the row says.
    label: Option<SharedString>,
    items: Vec<SidebarMenuItem>,
    /// True for the group at the top, which needs no space above it.
    leading: bool,
    collapsed: bool,
}

impl ScopeGroup {
    fn new(label: impl Into<SharedString>, items: Vec<SidebarMenuItem>) -> Self {
        Self {
            label: Some(label.into()),
            items,
            leading: false,
            collapsed: false,
        }
    }

    fn leading(items: Vec<SidebarMenuItem>) -> Self {
        Self {
            label: None,
            items,
            leading: true,
            collapsed: false,
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
        let label = self.label.clone().filter(|_| !collapsed);

        div()
            .flex()
            .flex_col()
            // Sections stand apart; rows within a section do not.
            .when(!leading, |this| this.pt_4())
            .when_some(label, |this, label| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .flex_shrink_0()
                        .h_7()
                        .px_2()
                        .text_xs()
                        .text_color(cx.theme().sidebar_foreground.opacity(0.7))
                        .child(label),
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
