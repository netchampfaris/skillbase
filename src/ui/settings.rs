//! The Settings pane: where everything lives, what the last scan could not
//! read, and the one preference the sidebar has.
//!
//! It replaces the list and the detail pane rather than opening as a dialog.
//! Two of its five sections are lists that can run long — one row per agent
//! directory, and the scan's warnings in full with their paths — and a dialog
//! would either crop them or grow a second scroll region over the top of the
//! work area. It is also a view of the machine rather than of the selected
//! skill, which is what the sidebar's other rows select, so selecting it there
//! and giving it the work area keeps one navigation model instead of two.
//!
//! Nothing here reads the filesystem. Whether a directory exists comes from
//! the scan, which runs on a background thread.

use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, ElementId, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div,
};
use skillbase_core::{Registry, UNSUPPORTED};

use crate::app::{ScanState, Skillbase};

use super::model::{DirStatus, HOME_OVERRIDE_ENV, Preferences, display_path};

impl Skillbase {
    pub(crate) fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let scan = self.scan();

        v_flex()
            .size_full()
            .min_w_0()
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .flex_shrink_0()
                    .h_12()
                    .px_5()
                    .items_center()
                    .child(div().text_base().font_medium().child("Settings")),
            )
            .child(
                v_flex()
                    .id("settings-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_5()
                    .pb_8()
                    .gap_6()
                    .child(self.directories_section(cx))
                    .child(self.sidebar_section(cx))
                    .child(self.warnings_section(cx))
                    .child(self.unsupported_section(cx))
                    .child(self.about_section(cx))
                    // A scan that has not landed yet leaves the two sections
                    // above it empty, so say so rather than show blanks.
                    .when(scan.is_none(), |this| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(match &self.scan {
                                    ScanState::Failed(error) => {
                                        SharedString::from(format!("The last scan failed: {error}"))
                                    }
                                    _ => "Scanning…".into(),
                                }),
                        )
                    }),
            )
    }

    /// The resolved store path and every agent directory, with whether each is
    /// on disk. This is the "where does all this actually live" answer.
    fn directories_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let dirs = self
            .scan()
            .map(|scan| scan.dirs.clone())
            .unwrap_or_default();
        let present = dirs.iter().filter(|dir| dir.exists).count();

        section(
            "Directories",
            format!(
                "{present} of {} exist on this machine. Skillbase reads all of them and writes \
                 only where you ask it to.",
                dirs.len()
            ),
            v_flex()
                .rounded(cx.theme().radius)
                .bg(cx.theme().group_box)
                .children(dirs.iter().map(|dir| self.dir_row(dir, cx)))
                .into_any_element(),
            cx,
        )
    }

    fn dir_row(&self, dir: &DirStatus, cx: &mut Context<Self>) -> AnyElement {
        let path = display_path(&dir.path, &self.roots);
        h_flex()
            .id(ElementId::from((ElementId::from("settings-dir"), dir.id)))
            .w_full()
            .px_3()
            .py_2()
            .gap_3()
            .items_center()
            .justify_between()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .w_40()
                    .flex_shrink_0()
                    .child(
                        Icon::new(if dir.exists {
                            IconName::CircleCheck
                        } else {
                            IconName::CircleX
                        })
                        .xsmall()
                        .text_color(if dir.exists {
                            cx.theme().success
                        } else {
                            cx.theme().muted_foreground
                        }),
                    )
                    .child(div().text_sm().truncate().child(dir.label.clone())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .truncate()
                    .child(path),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(if dir.exists { "on disk" } else { "not there" }),
            )
            .into_any_element()
    }

    fn sidebar_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let show_all = self.preferences.show_all_agents;
        let file = display_path(&Preferences::path(&self.roots), &self.roots);

        section(
            "Sidebar",
            format!("Kept in {file}."),
            v_flex()
                .gap_1()
                .child(
                    Switch::new("show-all-agents")
                        .checked(show_all)
                        .label("Show all agents")
                        .on_click(cx.listener(|this, checked: &bool, window, cx| {
                            this.set_show_all_agents(*checked, window, cx)
                        })),
                )
                .child(
                    div()
                        .pl_10()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(
                            "Off, the Agents group lists only the agents with a skills directory \
                             on this machine. On, it lists every agent Skillbase knows about and \
                             marks the ones that are not installed.",
                        ),
                )
                .into_any_element(),
            cx,
        )
    }

    /// The last scan's warnings, in full, with their paths.
    ///
    /// The list pane shows a one-line count of these; this is where the detail
    /// is reachable.
    fn warnings_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let home = self.roots.home().display().to_string();
        let warnings = self
            .scan()
            .map(|scan| scan.warnings.clone())
            .unwrap_or_default();

        let body = if warnings.is_empty() {
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("The last scan read every directory it opened.")
                .into_any_element()
        } else {
            v_flex()
                .p_3()
                .gap_2()
                .rounded(cx.theme().radius)
                .bg(cx.theme().group_box)
                .children(warnings.iter().map(|warning| {
                    h_flex()
                        .id(ElementId::from((
                            ElementId::from("scan-warning"),
                            warning.clone(),
                        )))
                        .w_full()
                        .gap_2()
                        .items_start()
                        .child(
                            Icon::new(IconName::TriangleAlert)
                                .xsmall()
                                .flex_shrink_0()
                                .text_color(cx.theme().warning),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                // Warnings are built in core, which has no
                                // notion of how a path is shown. Every other
                                // path in the interface is written with `~`;
                                // these should read the same way.
                                .child(warning.replace(&home, "~")),
                        )
                }))
                .into_any_element()
        };

        section(
            "Scan",
            format!(
                "{} problem{} during the last scan. Discovery never stops on one; it records it \
                 and carries on.",
                warnings.len(),
                if warnings.len() == 1 { "" } else { "s" }
            ),
            body,
            cx,
        )
    }

    /// The agents Skillbase deliberately does not support, with the reason.
    ///
    /// SPEC §2.2: naming them beats silently omitting them, because a user who
    /// wonders "where's Aider?" deserves an answer.
    fn unsupported_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        section(
            "Unsupported agents",
            format!(
                "Skillbase manages {} agents. These it deliberately does not, and says so rather \
                 than leaving them out.",
                Registry::all().len() - 1
            ),
            v_flex()
                .gap_3()
                .children(UNSUPPORTED.iter().map(|(name, reason)| {
                    v_flex()
                        .id(ElementId::from((ElementId::from("unsupported"), *name)))
                        .gap_1()
                        .child(div().text_sm().child(*name))
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(*reason),
                        )
                }))
                .into_any_element(),
            cx,
        )
    }

    fn about_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let home = self.roots.home().display().to_string();

        section(
            "About",
            format!("Skillbase {}", env!("CARGO_PKG_VERSION")),
            v_flex()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(if self.home_overridden {
                            format!(
                                "{HOME_OVERRIDE_ENV} is set, so every path above resolves under \
                                 {home} and every write lands there — not in your real home."
                            )
                        } else {
                            format!("Home directory: {home}")
                        }),
                )
                .into_any_element(),
            cx,
        )
    }
}

/// A titled section: a heading, a line of explanation, and its content.
fn section(
    title: &'static str,
    caption: String,
    body: AnyElement,
    cx: &mut Context<Skillbase>,
) -> impl IntoElement {
    v_flex()
        .flex_shrink_0()
        .gap_2()
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(title),
        )
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(caption),
        )
        .child(body)
}
