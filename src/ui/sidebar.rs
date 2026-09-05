//! The scope sidebar: a Library group, an Agents group listing only the agents
//! that exist on this machine, and Settings pinned to the bottom.

use gpui_kit::component::sidebar::{
    Sidebar, SidebarCollapsible, SidebarFooter, SidebarGroup, SidebarItem as _, SidebarMenu,
    SidebarMenuItem,
};
use gpui_kit::component::{ActiveTheme as _, Collapsible as _, IconName};
use gpui_kit::{App, Context, Div, IntoElement, ParentElement as _, Styled as _, Window, div, px};

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
        let scan = self.scan();

        let library = Library::ALL.map(|library| {
            let target = Scope::Library(library);
            let total = scan.map(|scan| scan.count(target));
            SidebarMenuItem::new(library.label())
                .icon(library_icon(library))
                .active(scope == target)
                .suffix(move |_, cx| count_label(total, cx))
                .on_click(
                    cx.listener(move |this, _, window, cx| this.select_scope(target, window, cx)),
                )
        });

        // Only agents whose global directory exists. An agent that is not
        // installed is hidden rather than greyed, per the specification.
        let agents: Vec<_> =
            scan.map(|scan| scan.installed.clone())
                .unwrap_or_default()
                .into_iter()
                .map(|agent| {
                    let target = Scope::Agent(agent.id);
                    let total = scan.map(|scan| scan.count(target));
                    SidebarMenuItem::new(agent.display_name)
                        .icon(IconName::Bot)
                        .active(scope == target)
                        .suffix(move |_, cx| count_label(total, cx))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.select_scope(target, window, cx)
                        }))
                })
                .collect();

        let settings = SidebarMenu::new()
            .child(
                SidebarMenuItem::new("Settings")
                    .icon(IconName::Settings)
                    .on_click(|_, _, _| {}),
            )
            .collapsed(collapsed)
            .render("settings", window, cx);

        Sidebar::new("scopes")
            .collapsible(SidebarCollapsible::Icon)
            .collapsed(collapsed)
            .w(px(SIDEBAR_WIDTH))
            .child(SidebarGroup::new("Library").child(SidebarMenu::new().children(library)))
            .child(SidebarGroup::new("Agents").child(SidebarMenu::new().children(agents)))
            .footer(SidebarFooter::new().child(settings))
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
