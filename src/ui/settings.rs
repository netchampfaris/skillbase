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

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, ElementId, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use skillbase_core::{GITHUB_TOKEN_ENV, Registry, TokenSource, UNSUPPORTED};

use crate::app::{ScanState, Skillbase, UpdateState};

use super::PAGE_MAX_WIDTH;
use super::model::{DirStatus, HOME_OVERRIDE_ENV, Preferences, display_path, in_words};
use super::tooltip::text_tooltip;

impl Skillbase {
    pub(crate) fn render_settings(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let scan = self.scan();

        // Settings takes the whole work area, so with the sidebar hidden this
        // band is the leftmost one and has to leave the traffic lights room.
        let title_row = h_flex()
            .h_full()
            .w_full()
            .px_5()
            .gap_2()
            .items_center()
            .children(self.sidebar_reopen(cx))
            .child(div().text_base().font_medium().child("Settings"));

        v_flex()
            .size_full()
            .min_w_0()
            .bg(cx.theme().background)
            .child(self.column_band("settings-band", title_row, window, cx))
            .child(
                v_flex()
                    .id("settings-body")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .items_center()
                    .overflow_y_scrollbar()
                    .child(
                        v_flex()
                            // Full width up to the cap rather than a fixed
                            // width: at the 860px minimum window the work
                            // area is narrower than PAGE_MAX_WIDTH, and a
                            // fixed w() would overflow it.
                            .w_full()
                            .max_w(px(PAGE_MAX_WIDTH))
                            .px_5()
                            .pb_8()
                            .gap_6()
                            .child(self.directories_section(cx))
                            .child(self.sidebar_section(cx))
                            .child(self.usage_section(cx))
                            .child(self.updates_section(cx))
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
                                            ScanState::Failed(error) => SharedString::from(
                                                format!("The last scan failed: {error}"),
                                            ),
                                            _ => "Scanning…".into(),
                                        }),
                                )
                            }),
                    ),
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
            // Directories, not agents, and it has to say so: the sidebar
            // counts agents over a different total, and two bare fractions on
            // two screens read as a contradiction. An agent that is installed
            // can still have no skills directory, which is the second
            // sentence.
            format!(
                "{present} of these {} directories exist on this machine. Skillbase reads all of \
                 them and writes only where you ask it to. An agent that is installed can still \
                 have no directory here: it is created the first time the agent is given a skill.",
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
        let full = path.clone();
        h_flex()
            .id(ElementId::from((ElementId::from("settings-dir"), dir.id)))
            .w_full()
            .px_3()
            .py_2()
            .gap_3()
            .items_center()
            .justify_between()
            // Truncation clips the end of the path, which is the part that
            // says which directory this is. The row is what Settings exists
            // to answer, so the whole path stays reachable.
            .tooltip(text_tooltip(full))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .w_40()
                    .flex_shrink_0()
                    .child(
                        // Presence, not approval. A tick is what the sidebar
                        // uses for "Managed", and this row is answering a
                        // different question: whether the directory is there.
                        // The drive reads as the "on disk" the row says.
                        Icon::new(if dir.exists {
                            IconName::HardDrive
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
                        .small()
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

    /// Where the "Most used" ordering gets its numbers, and what it could not
    /// read.
    ///
    /// The list header's help names the groups; this is where the per-source
    /// figures and any unreadable session file are reachable. Without it a
    /// count that is quietly missing a source looks exactly like a skill
    /// nobody has run.
    fn usage_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let home = self.roots.home().display().to_string();
        let (caption, body) = match self.usage.as_ref() {
            None => (
                "Reading the session records…".to_string(),
                div().into_any_element(),
            ),
            Some(usage) => {
                let caption = if usage.is_empty() {
                    "No agent on this machine keeps session records Skillbase can read, so \
                     every skill counts zero."
                        .to_string()
                } else {
                    format!(
                        "{} invocation{} across {} file{}. Only agents that keep a session \
                         transcript can be counted, and Claude Code prunes its own \
                         after {} days, so this is recent history rather than a \
                         lifetime total.",
                        usage.total(),
                        if usage.total() == 1 { "" } else { "s" },
                        usage.files_read(),
                        if usage.files_read() == 1 { "" } else { "s" },
                        skillbase_core::CLAUDE_RETENTION_DAYS,
                    )
                };

                let body = v_flex()
                    .gap_2()
                    .child(
                        v_flex()
                            .rounded(cx.theme().radius)
                            .bg(cx.theme().group_box)
                            .children(usage.sources().iter().map(|stat| {
                                let path = display_path(&stat.source.dir(&self.roots), &self.roots);
                                let full = path.clone();
                                h_flex()
                                    .id(ElementId::from((
                                        ElementId::from("usage-source"),
                                        stat.source.agent_id(),
                                    )))
                                    .w_full()
                                    .px_3()
                                    .py_2()
                                    .gap_3()
                                    .items_center()
                                    // The truncated end of the path is the part
                                    // that identifies the directory.
                                    .tooltip(text_tooltip(full))
                                    .child(
                                        div()
                                            .w_40()
                                            .flex_shrink_0()
                                            .text_sm()
                                            .truncate()
                                            .child(stat.source.display_name()),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_sm()
                                            .truncate()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(path),
                                    )
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(format!(
                                                "{} in {} file{}",
                                                stat.invocations,
                                                stat.files,
                                                if stat.files == 1 { "" } else { "s" }
                                            )),
                                    )
                            })),
                    )
                    .children(usage.warnings().iter().map(|warning| {
                        h_flex()
                            .id(ElementId::from((
                                ElementId::from("usage-warning"),
                                SharedString::from(warning.clone()),
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
                                    .child(warning.replace(&home, "~")),
                            )
                    }))
                    .into_any_element();
                (caption, body)
            }
        };

        section("Usage", caption, body, cx)
    }

    /// What the update check costs and what it last found.
    ///
    /// GitHub allows sixty requests an hour without a token, which a few dozen
    /// repositories fit inside exactly once. That makes the budget worth
    /// showing rather than leaving the user to discover it as a check that
    /// quietly stops working.
    fn updates_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let checking = matches!(self.updates, UpdateState::Checking);
        let caption = match self.token_source {
            // The lookup runs `gh auth token`, which can wait on the keychain,
            // so it is a background task and this section can be read before it
            // lands. Saying so beats naming a limit that may be wrong.
            None => "Looking for a GitHub token. A check asks once per repository and only walks \
                     a repository that has moved."
                .to_string(),
            Some(TokenSource::Environment) => format!(
                "{GITHUB_TOKEN_ENV} is set, so GitHub allows 5000 requests an hour. A check asks \
                 once per repository and only walks a repository that has moved."
            ),
            Some(TokenSource::GitHubCli) => {
                "Using the token from GitHub CLI, so GitHub allows 5000 requests an hour. The \
                 token stays in memory and is not written to disk. A check asks once per \
                 repository and only walks a repository that has moved."
                    .to_string()
            }
            Some(TokenSource::None) => format!(
                "No token, so GitHub allows 60 requests an hour. A check asks once per \
                 repository and only walks a repository that has moved. Set {GITHUB_TOKEN_ENV}, \
                 or log in with gh, to raise the limit to 5000."
            ),
        };

        let budget: SharedString = match self.rate_limit {
            Some(limit) => format!(
                "{} of {} requests left, resetting in {}",
                limit.remaining,
                limit.limit,
                in_words(limit.wait_from(std::time::SystemTime::now()).as_secs())
            )
            .into(),
            None => "GitHub has not been asked yet, so there is no figure for the budget".into(),
        };

        let found: SharedString = match &self.updates {
            UpdateState::Idle => "No check has run yet".into(),
            UpdateState::Checking => "Checking…".into(),
            UpdateState::Ready(report) => {
                let updatable = report.updatable().count();
                format!(
                    "{} repositor{} checked in {} request{}; {} skill{} behind upstream",
                    report.repos_checked(),
                    if report.repos_checked() == 1 {
                        "y"
                    } else {
                        "ies"
                    },
                    report.requests(),
                    if report.requests() == 1 { "" } else { "s" },
                    updatable,
                    if updatable == 1 { "" } else { "s" },
                )
                .into()
            }
        };

        let updatable = self.updatable_count();
        let downloading = self.downloading();
        // The lookup is the one thing on this row the user can start again,
        // and it sits on the row it changes rather than beside the update
        // commands, which mean something else entirely.
        let look_again = Button::new("look-for-token")
            .ghost()
            .xsmall()
            .label("Look again")
            .disabled(self.token_source.is_none())
            .on_click(cx.listener(|this, _, window, cx| this.refresh_token_source(window, cx)))
            .into_any_element();

        section(
            "Updates",
            caption,
            v_flex()
                .gap_3()
                .child(
                    v_flex()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().group_box)
                        .child(fact_row(
                            "token",
                            "GitHub token",
                            match self.token_source {
                                None => "still looking",
                                Some(TokenSource::Environment) => "found in the environment",
                                Some(TokenSource::GitHubCli) => "using GitHub CLI",
                                Some(TokenSource::None) => "not set",
                            },
                            Some(look_again),
                            cx,
                        ))
                        .child(fact_row("budget", "Rate limit", budget, None, cx))
                        .child(fact_row("last-check", "Last check", found, None, cx)),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("check-updates")
                                .outline()
                                .small()
                                .label("Check for updates now")
                                .disabled(checking)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.check_for_updates(window, cx)
                                })),
                        )
                        .child(
                            Button::new("update-all")
                                .outline()
                                .small()
                                .label("Update all")
                                .disabled(updatable == 0 || checking || downloading)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.update_all(window, cx)),
                                ),
                        ),
                )
                // Only when there is something to take. Update all deletes
                // directories, so what it will and will not touch has to be
                // readable before the click rather than reported after it.
                .when(updatable > 0, |this| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                "Update all downloads each of them again and writes it over its \
                                 own directory. A copy that has been edited since it was \
                                 installed, or that has no install record, is left for its own \
                                 page, where the change is spelled out first.",
                            ),
                    )
                })
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

/// One labelled fact, on the same two lanes the directory rows use so the two
/// lists read as one column, with an optional control on the trailing edge for
/// the fact the user can change.
fn fact_row(
    id: &'static str,
    label: &'static str,
    value: impl Into<SharedString>,
    action: Option<AnyElement>,
    cx: &mut Context<Skillbase>,
) -> AnyElement {
    h_flex()
        .id(ElementId::from((ElementId::from("settings-fact"), id)))
        .w_full()
        .px_3()
        // Shorter than the other rows so that a row carrying a button is the
        // same height as one that does not, and the column keeps its rhythm.
        .py_1()
        .min_h(px(36.))
        .gap_3()
        .items_center()
        .child(
            div()
                .w_40()
                .flex_shrink_0()
                .text_sm()
                .truncate()
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(value.into()),
        )
        .children(action.map(|action| div().flex_shrink_0().child(action)))
        .into_any_element()
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
                // The caption directly under it is a step larger and the same
                // colour, so without the weight the heading is the quieter of
                // the two lines and stops reading as a heading at all.
                .font_medium()
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
