//! The Discover view: search skills.sh, and install what it finds.
//!
//! It replaces the list and the detail pane rather than living inside them.
//! The three-pane layout is about skills that are already on this machine —
//! the list filters them, the detail pane edits one — and nothing here is on
//! the machine yet. A row is a candidate, not a skill.
//!
//! Two things reach the network from this module, and both are on background
//! tasks: the search, and the install that follows it. skills.sh answers the
//! first; every byte of a skill comes from GitHub, because the registry is a
//! index and not a mirror.

use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogClose, DialogFooter};
use gpui_kit::component::input::Input;
use gpui_kit::component::label::Label;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, AppContext as _, Context, ElementId, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, Styled as _, Window, div, px, rems,
};
use skillbase_core::{
    DEFAULT_LIMIT, GitHub, InstallOptions, MIN_QUERY_LEN, SearchHit, SearchResults, SkillLocation,
    SkillsSh, UreqHttp, resolve,
};

use crate::app::{Skillbase, WorkArea};

use super::model::display_path;
use super::{PAGE_MAX_WIDTH, install_skill, report_install};

/// How long typing has to stop before a search is sent.
///
/// Long enough that a typed word is one request rather than one per letter,
/// short enough that the results feel like a response to the typing.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// Where the registry search has got to.
pub(crate) enum SearchState {
    /// Nothing has been asked for, or the query is too short to ask about.
    Idle,
    Searching,
    Ready(Rc<SearchResults>),
    Failed(SharedString),
}

impl Skillbase {
    pub(crate) fn render_discover(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // Discover takes the whole work area, so with the sidebar hidden this
        // band is the leftmost one and has to leave the traffic lights room.
        let title_row = h_flex()
            .h_full()
            .w_full()
            .px_5()
            .gap_2()
            .items_center()
            .children(self.sidebar_reopen(cx))
            .child(div().text_base().font_medium().child("Discover"));

        v_flex()
            .size_full()
            .min_w_0()
            .bg(cx.theme().background)
            .child(self.column_band("discover-band", title_row, window, cx))
            .child(
                v_flex().flex_1().min_h_0().w_full().items_center().child(
                    v_flex()
                        .flex_1()
                        .min_h_0()
                        // Full width up to the cap rather than a fixed width:
                        // at the 860px minimum window the work area is
                        // narrower than PAGE_MAX_WIDTH, and a fixed w() would
                        // overflow it.
                        .w_full()
                        .max_w(px(PAGE_MAX_WIDTH))
                        .child(
                            h_flex().flex_shrink_0().h_11().px_5().items_center().child(
                                div().flex_1().min_w_0().child(
                                    Input::new(&self.discover_query)
                                        .small()
                                        .cleanable(true)
                                        .prefix(Icon::new(IconName::Search).small()),
                                ),
                            ),
                        )
                        .child(
                            div()
                                .id("discover-body")
                                .flex_1()
                                .min_h_0()
                                // With a bar: a full page of results runs
                                // well past the fold with nothing to say so.
                                .overflow_y_scrollbar()
                                .px_5()
                                .pb_8()
                                .child(self.discover_body(cx)),
                        ),
                ),
            )
    }

    fn discover_body(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.discover {
            SearchState::Idle => empty_state(
                "Search skills.sh",
                "Type at least two characters. The results come from the skills.sh registry; \
                 installing one downloads it from the repository the registry names."
                    .into(),
                cx,
            ),
            SearchState::Searching => v_flex()
                .py_2()
                .gap_4()
                .children((0..5).map(|row| {
                    v_flex()
                        .id(ElementId::from(("discover-skeleton", row as usize)))
                        .gap_2()
                        .child(Skeleton::new().h(rems(0.9)).w(rems(11.)))
                        .child(Skeleton::new().h(rems(0.8)).w(rems(16.)))
                }))
                .into_any_element(),
            SearchState::Failed(error) => {
                empty_state("skills.sh did not answer", error.clone(), cx)
            }
            SearchState::Ready(results) if results.is_empty() => empty_state(
                "Nothing matched",
                format!("skills.sh lists no skill for “{}”.", results.query).into(),
                cx,
            ),
            SearchState::Ready(results) => v_flex()
                .py_2()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "{} result{} from skills.sh. Installing one writes it into {}.",
                            results.skills.len(),
                            if results.skills.len() == 1 { "" } else { "s" },
                            display_path(&self.roots.store_dir(), &self.roots)
                        )),
                )
                .child(
                    v_flex()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().group_box)
                        .children(results.skills.iter().map(|hit| self.result_row(hit, cx))),
                )
                .into_any_element(),
        }
    }

    /// One search result.
    ///
    /// skills.sh returns no description, so there is nothing to put under the
    /// name. What the row can answer instead is which repository it comes from
    /// and how many people have installed it, which is the whole basis for
    /// choosing between two rows with similar names.
    fn result_row(&self, hit: &SearchHit, cx: &mut Context<Self>) -> AnyElement {
        let hit = hit.clone();
        let installing = self.installing;

        h_flex()
            // Identity comes from the registry's own row id, so a row keeps its
            // state as results are replaced.
            .id(ElementId::from((
                ElementId::from("discover-row"),
                hit_id(&hit),
            )))
            .w_full()
            .px_3()
            .py_2()
            .gap_3()
            .items_center()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .text_sm()
                            .font_medium()
                            .truncate()
                            .child(SharedString::from(hit.name.clone())),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(SharedString::from(hit.source.clone())),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(installs(hit.installs)),
            )
            .child(
                Button::new(ElementId::from((
                    ElementId::from("discover-install"),
                    hit_id(&hit),
                )))
                .outline()
                .small()
                .label("Install")
                .disabled(installing)
                .on_click(
                    cx.listener(move |this, _, window, cx| {
                        this.install_hit(hit.clone(), window, cx)
                    }),
                ),
            )
            .into_any_element()
    }

    /// Ask skills.sh what matches what has been typed, once the typing stops.
    ///
    /// Every keystroke calls this. The generation counter is what makes that
    /// safe: it cancels the pending request before it is sent and drops a
    /// result that lands after a newer keystroke, so the rows always belong to
    /// the query in the field.
    pub(crate) fn search_registry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.discover_generation += 1;
        let generation = self.discover_generation;

        let query = self.discover_query.read(cx).value().trim().to_string();
        if query.chars().count() < MIN_QUERY_LEN {
            // The API refuses a shorter query, so the interface says what it
            // is waiting for rather than showing an error it caused itself.
            self.discover = SearchState::Idle;
            cx.notify();
            return;
        }

        self.discover = SearchState::Searching;
        cx.notify();

        self._discover_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let current = this
                .read_with(cx, |this, _| this.discover_generation == generation)
                .unwrap_or(false);
            if !current {
                return;
            }

            let results = cx
                .background_spawn(async move {
                    SkillsSh::new(UreqHttp::new()).search(&query, DEFAULT_LIMIT, None)
                })
                .await;

            this.update(cx, |this, cx| {
                if this.discover_generation != generation {
                    return;
                }
                this.discover = match results {
                    Ok(results) => SearchState::Ready(Rc::new(results)),
                    Err(error) => SearchState::Failed(error.to_string().into()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Work out where a search result actually lives, then install it.
    ///
    /// A hit names a repository and a directory name and nothing else, so the
    /// repository has to be looked at. It can hold more than one directory of
    /// that name, and only the user knows which was meant.
    fn install_hit(&mut self, hit: SearchHit, window: &mut Window, cx: &mut Context<Self>) {
        if self.installing {
            return;
        }
        self.installing = true;
        cx.notify();

        let name = SharedString::from(hit.name.clone());
        let source = SharedString::from(hit.source.clone());
        cx.spawn_in(window, async move |this, cx| {
            let resolved = cx
                .background_spawn(
                    async move { resolve(&GitHub::from_env(UreqHttp::new()), &hit, None) },
                )
                .await;

            this.update_in(cx, |this, window, cx| {
                this.installing = false;
                cx.notify();
                match resolved {
                    Ok(locations) if locations.is_empty() => {
                        window.push_notification(
                            Notification::error(format!(
                                "{source} holds no directory called {name} with a SKILL.md in \
                                 it. The registry and the repository disagree, usually because \
                                 it was renamed or removed upstream."
                            ))
                            .title("Nothing to install")
                            // Nothing was installed, so the list looks exactly
                            // as it did before the click. This sentence is the
                            // only thing that says why, and it stays until it
                            // is dismissed.
                            .autohide(false),
                            cx,
                        );
                    }
                    Ok(locations) if locations.len() == 1 => {
                        this.install_location(locations[0].clone(), window, cx)
                    }
                    Ok(locations) => this.open_location_dialog(name, locations, window, cx),
                    Err(error) => {
                        window.push_notification(
                            Notification::error(error.to_string())
                                .title("Could not install")
                                .autohide(false),
                            cx,
                        );
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Ask which of a repository's directories was meant.
    ///
    /// Two directories named `pdf` in one repository is a real thing, so the
    /// choice is put to the user rather than guessed at. Each row names the
    /// path within the repository, which is the only thing that tells them
    /// apart.
    fn open_location_dialog(
        &mut self,
        name: SharedString,
        locations: Vec<SkillLocation>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let locations = locations.clone();
            let this = this.clone();
            let count = locations.len();

            dialog
                .title(format!("Which {name}?"))
                .width(px(520.))
                .content({
                    let name = name.clone();
                    move |content, _, cx| {
                        let locations = locations.clone();
                        let this = this.clone();
                        content.child(
                            v_flex()
                                .p_4()
                                .gap_3()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(format!(
                                            "{} holds {count} directories called {name}. Pick the \
                                         one to download.",
                                            locations
                                                .first()
                                                .map(|location| location.repo.slug())
                                                .unwrap_or_default()
                                        )),
                                )
                                .child(
                                    v_flex()
                                        .rounded(cx.theme().radius)
                                        .bg(cx.theme().group_box)
                                        .children(locations.iter().map(|location| {
                                            let chosen = location.clone();
                                            let this = this.clone();
                                            let path =
                                                SharedString::from(if location.path.is_empty() {
                                                    "the repository root".to_string()
                                                } else {
                                                    location.path.clone()
                                                });
                                            h_flex()
                                                .id(ElementId::from((
                                                    ElementId::from("install-choice"),
                                                    path.clone(),
                                                )))
                                                .w_full()
                                                .px_3()
                                                .py_2()
                                                .gap_3()
                                                .items_center()
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .text_sm()
                                                        .truncate()
                                                        .child(path.clone()),
                                                )
                                                .child(
                                                    Button::new(ElementId::from((
                                                        ElementId::from("install-choice-button"),
                                                        path,
                                                    )))
                                                    .outline()
                                                    .small()
                                                    .label("Install")
                                                    .on_click(move |_, window, cx| {
                                                        let chosen = chosen.clone();
                                                        this.update(cx, |this, cx| {
                                                            this.install_location(
                                                                chosen, window, cx,
                                                            )
                                                        })
                                                        .ok();
                                                        window.close_dialog(cx);
                                                    }),
                                                )
                                        })),
                                ),
                        )
                    }
                })
                .footer(
                    DialogFooter::new().p_4().child(
                        DialogClose::new().child(
                            Button::new("cancel-install-choice")
                                .outline()
                                .label("Cancel"),
                        ),
                    ),
                )
        });
    }

    /// Ask for a repository, then download whatever it names.
    ///
    /// One field, because every spelling this accepts is one string. The forms
    /// are listed under it rather than split across several fields: a user who
    /// has a URL on the clipboard should be able to paste it.
    pub(crate) fn open_install_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.install_spec
            .update(cx, |state, cx| state.set_value("", window, cx));

        let field = self.install_spec.clone();
        let store = display_path(&self.roots.store_dir(), &self.roots);
        let this = cx.entity().downgrade();

        window.open_dialog(cx, move |dialog, _, _| {
            let field = field.clone();
            let store = store.clone();
            let this = this.clone();
            dialog
                .title("Install from GitHub")
                .width(px(460.))
                .content(move |content, _, cx| {
                    let store = store.clone();
                    content.child(
                        v_flex().p_4().gap_4().child(
                            v_flex()
                                .gap_2()
                                .child(Label::new("Repository"))
                                .child(Input::new(&field).small())
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(
                                            "owner/repo, owner/repo@branch, \
                                             owner/repo/path/to/skill, a github.com URL, or an \
                                             SSH remote.",
                                        ),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(format!(
                                            "The skill is downloaded into {store}, where every \
                                             agent that reads that directory can see it."
                                        )),
                                ),
                        ),
                    )
                })
                .footer(
                    DialogFooter::new()
                        .p_4()
                        .child(
                            DialogClose::new()
                                .child(Button::new("cancel-install").outline().label("Cancel")),
                        )
                        .child(
                            Button::new("confirm-install")
                                .primary()
                                .label("Install")
                                .on_click(move |_, window, cx| {
                                    this.update(cx, |this, cx| this.install_from_spec(window, cx))
                                        .ok();
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    fn install_from_spec(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let spec = self.install_spec.read(cx).value().trim().to_string();
        let Some(location) = SkillLocation::parse(&spec) else {
            window.push_notification(
                Notification::error(if spec.is_empty() {
                    "Name a repository first, as owner/repo or as a github.com URL.".to_string()
                } else {
                    format!(
                        "`{spec}` does not name a GitHub repository. Try owner/repo, or paste \
                         the repository's URL."
                    )
                })
                .title("Could not install")
                // What the user typed is still in the field, and this says
                // what is wrong with it. It stays until it is dismissed.
                .autohide(false),
                cx,
            );
            return;
        };
        self.install_location(location, window, cx);
    }

    /// Download one skill and adopt it.
    ///
    /// The download, the extraction and the write into the store all happen on
    /// a background task. What lands on the main thread is the result, and
    /// either way it is said out loud: a failure here leaves the list exactly
    /// as it was, so without a sentence there is nothing to say why.
    pub(crate) fn install_location(
        &mut self,
        location: SkillLocation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.installing {
            return;
        }
        self.installing = true;
        cx.notify();

        let roots = self.roots.clone();
        cx.spawn_in(window, async move |this, cx| {
            let installed = cx
                .background_spawn(async move {
                    install_skill(&roots, &location, &InstallOptions::new())
                })
                .await;

            this.update_in(cx, |this, window, cx| {
                this.installing = false;
                cx.notify();
                if let Some(name) = report_install(
                    "Installed",
                    "Could not install",
                    installed,
                    &this.roots,
                    window,
                    cx,
                ) {
                    this.installed(name, window, cx);
                }
            })
            .ok();
        })
        .detach();
    }
}

/// A stable identity for a search result row.
///
/// The registry's own row id where there is one, and the repository and skill
/// id where there is not — never the row's position, which changes with every
/// keystroke.
fn hit_id(hit: &SearchHit) -> SharedString {
    if !hit.id.is_empty() {
        return hit.id.clone().into();
    }
    format!("{}/{}", hit.source, hit.skill_id).into()
}

/// How many installs the registry has seen, with the digits grouped so two
/// rows can be compared at a glance.
fn installs(count: u64) -> SharedString {
    let digits = count.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("{grouped} install{}", if count == 1 { "" } else { "s" }).into()
}

fn empty_state(
    title: &'static str,
    detail: SharedString,
    cx: &mut Context<Skillbase>,
) -> AnyElement {
    v_flex()
        .py_8()
        .gap_1()
        // The title carries the weight rather than a larger size: at this size
        // the explanation under it is the same `text_sm`, and without the
        // weight the two lines read as one paragraph with no heading.
        .child(div().text_sm().font_medium().child(title))
        .child(
            div()
                .max_w(rems(32.))
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(detail),
        )
        .into_any_element()
}

impl Skillbase {
    /// Give the work area to Discover.
    pub(crate) fn show_discover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show(WorkArea::Discover, cx);
        self.discover_query
            .update(cx, |state, cx| state.focus(window, cx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_install_count_groups_its_digits() {
        assert_eq!(installs(0), "0 installs");
        assert_eq!(installs(1), "1 install");
        assert_eq!(installs(999), "999 installs");
        assert_eq!(installs(1_204), "1,204 installs");
        assert_eq!(installs(1_000_000), "1,000,000 installs");
    }

    #[test]
    fn a_row_is_identified_by_the_registry_and_never_by_its_position() {
        let mut hit = SearchHit {
            id: "row-7".into(),
            skill_id: "pdf".into(),
            name: "PDF".into(),
            installs: 3,
            source: "anthropics/skills".into(),
        };
        assert_eq!(hit_id(&hit), "row-7");
        hit.id = String::new();
        assert_eq!(hit_id(&hit), "anthropics/skills/pdf");
    }
}
