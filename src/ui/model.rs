//! The view model: one read-only scan of the filesystem, mapped into the shape
//! the three panes render.
//!
//! Everything here is plain data and pure functions. The scan itself is the one
//! function that touches the filesystem, and it is called from a background
//! task, never from `render`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use gpui_kit::SharedString;
use skillbase_core::{
    AgentDef, DisableMode, DiscoveredSkill, Discovery, Location, LocationKind, PRIVATE_ID,
    Provenance, Registry, Roots, SHARED_ID, SkillError, SkillLock, UpdateTarget, provenance_of,
};

/// Environment variable that points Skillbase at a different home directory.
///
/// Every path the application reads or writes is resolved from one [`Roots`],
/// so setting this redirects discovery *and* every mutation together. It exists
/// so that link, adopt, release and delete can be exercised against a throwaway
/// tree instead of the user's real skills.
pub const HOME_OVERRIDE_ENV: &str = "SKILLBASE_HOME";

/// The home directory Skillbase runs against, and whether it was overridden.
///
/// A `true` second element means the application is not looking at the real
/// home, which the title bar says out loud.
pub fn resolve_roots() -> Result<(Roots, bool), SkillError> {
    match std::env::var_os(HOME_OVERRIDE_ENV) {
        Some(home) if !home.is_empty() => Ok((Roots::new(PathBuf::from(home)), true)),
        _ => Ok((Roots::discover()?, false)),
    }
}

/// The file the "Show all agents" preference is kept in.
///
/// It sits beside the store rather than in the platform's configuration
/// directory, so that it travels with [`HOME_OVERRIDE_ENV`]: a run against a
/// throwaway home gets its own preferences and cannot rewrite the real ones.
pub const SETTINGS_FILE: &str = ".skillbase/settings.json";

/// How the skill list is ordered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SkillSort {
    /// Alphabetical, which is the order a directory listing gives and the one
    /// a reader can predict.
    #[default]
    Name,
    /// Most-invoked first, from the session records agents leave behind.
    MostUsed,
}

impl SkillSort {
    /// Every ordering, in the order the menu lists them.
    pub const ALL: [SkillSort; 2] = [SkillSort::Name, SkillSort::MostUsed];

    /// What the menu calls it.
    pub fn label(self) -> &'static str {
        match self {
            SkillSort::Name => "Name",
            SkillSort::MostUsed => "Most used",
        }
    }

    /// How it is spelled in the settings file.
    fn key(self) -> &'static str {
        match self {
            SkillSort::Name => "name",
            SkillSort::MostUsed => "most-used",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|sort| sort.key() == key)
    }
}

/// The preferences Skillbase keeps between runs.
///
/// Written as JSON so the file is readable and can grow keys later without a
/// format change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Preferences {
    /// Show every agent in the sidebar, including those with no directory on
    /// this machine. Off by default, per SPEC §5.1.
    pub show_all_agents: bool,
    /// How the skill list is ordered.
    pub sort: SkillSort,
}

impl Preferences {
    /// Where the file lives under this home.
    pub fn path(roots: &Roots) -> PathBuf {
        roots.home().join(SETTINGS_FILE)
    }

    /// Reads the preferences, falling back to the defaults for anything the
    /// file does not say.
    ///
    /// A missing, unreadable or malformed file is not an error worth
    /// interrupting the user for: it means "no preference expressed".
    pub fn load(roots: &Roots) -> Self {
        let text = fs::read_to_string(Self::path(roots)).unwrap_or_default();
        Self {
            show_all_agents: json_bool(&text, "show_all_agents").unwrap_or(false),
            sort: json_string(&text, "sort")
                .and_then(|value| SkillSort::from_key(&value))
                .unwrap_or_default(),
        }
    }

    /// Writes the preferences, creating `~/.skillbase` if it is missing.
    ///
    /// Blocking. Call it from a background task.
    pub fn save(&self, roots: &Roots) -> std::io::Result<PathBuf> {
        let path = Self::path(roots);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            &path,
            format!(
                "{{\n  \"show_all_agents\": {},\n  \"sort\": \"{}\"\n}}\n",
                self.show_all_agents,
                self.sort.key()
            ),
        )?;
        Ok(path)
    }
}

/// Reads one top-level value out of the settings file.
///
/// Deliberately narrow: these two understand exactly what [`Preferences::save`]
/// writes — `"key": true` and `"key": "value"` — and treat everything else as
/// absent rather than as an error. A whole JSON parser would be a dependency
/// bought for two settings, and a preference that fails to parse should fall
/// back to the default rather than stop the application.
fn json_bool(text: &str, key: &str) -> Option<bool> {
    let rest = after_key(text, key)?;
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn json_string(text: &str, key: &str) -> Option<String> {
    let rest = after_key(text, key)?.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// The text following `"key":`, with leading whitespace removed.
fn after_key<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let quoted = format!("\"{key}\"");
    let rest = text.split_once(&quoted)?.1;
    Some(rest.trim_start().strip_prefix(':')?.trim_start())
}

/// One directory Skillbase reads, and whether it is there.
///
/// This is the "where does all this actually live" answer the Settings pane
/// gives. The `exists` flag is read once, on the scan's background thread,
/// because `render` may not touch the filesystem.
#[derive(Clone, Debug)]
pub struct DirStatus {
    /// The scope id: [`PRIVATE_ID`], or an agent id.
    pub id: &'static str,
    /// What to call it on screen.
    pub label: SharedString,
    pub path: PathBuf,
    pub exists: bool,
}

/// The rows of the sidebar's Library group.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Library {
    All,
    Shared,
    Managed,
    Unmanaged,
    Invalid,
    Conflicts,
}

impl Library {
    pub const ALL: [Library; 6] = [
        Library::All,
        Library::Shared,
        Library::Managed,
        Library::Unmanaged,
        Library::Invalid,
        Library::Conflicts,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Library::All => "All",
            Library::Shared => "Shared",
            Library::Managed => "Managed",
            Library::Unmanaged => "Unmanaged",
            Library::Invalid => "Invalid",
            // "Duplicates" says what the group holds. The detail pane calls
            // the same thing a duplicate copy, and one operation should not
            // have two names.
            Library::Conflicts => "Duplicates",
        }
    }

    /// How a skill is recognised as this group. Shown in the list header's
    /// help, so the wording matches what the filter actually tests.
    pub fn explanation(self) -> &'static str {
        match self {
            Library::All => "Every skill Skillbase found on this machine.",
            Library::Shared => {
                "The origin is in ~/.agents/skills, which most agents read on their own. \
                 That directory is Skillbase's store, so a skill here is already managed."
            }
            Library::Managed => {
                "The origin is in the store or in ~/.skillbase/private. Skillbase owns \
                 the directory, so it can add and remove the links other agents follow."
            }
            Library::Unmanaged => {
                "The origin sits somewhere Skillbase does not own. You can read and edit \
                 it in place; visibility stays as you found it until you Adopt it."
            }
            Library::Invalid => {
                "SKILL.md is not a frontmatter document Skillbase can parse. The skill \
                 still appears so you can open the file and fix it."
            }
            Library::Conflicts => {
                "The same name exists as more than one real directory. Copies that \
                 drifted are listed rather than silently merged."
            }
        }
    }

    fn index(self) -> usize {
        Library::ALL.iter().position(|l| *l == self).unwrap_or(0)
    }

    fn matches(self, skill: &SkillView) -> bool {
        match self {
            Library::All => true,
            Library::Shared => skill.in_shared,
            Library::Managed => skill.managed,
            Library::Unmanaged => !skill.managed,
            Library::Invalid => skill.parse_error.is_some(),
            Library::Conflicts => !skill.conflicts.is_empty(),
        }
    }
}

/// What the skill list is currently showing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Library(Library),
    /// Every skill the agent with this id can currently see.
    Agent(&'static str),
}

impl Scope {
    /// The heading the title bar shows for this scope.
    pub fn title(self) -> &'static str {
        match self {
            Scope::Library(library) => library.label(),
            Scope::Agent(id) => agent_label(id),
        }
    }

    /// Whether this scope lists the given skill.
    pub fn shows(self, skill: &SkillView) -> bool {
        match self {
            Scope::Library(library) => library.matches(skill),
            Scope::Agent(id) => skill.visible_to.contains(&id),
        }
    }
}

/// An agent's display name, by id.
pub fn agent_label(id: &str) -> &'static str {
    Registry::get(id)
        .map(|agent| agent.display_name)
        .unwrap_or("Unknown")
}

/// One validation finding, ready to render beside the field it concerns.
#[derive(Clone, Debug)]
pub struct Issue {
    /// The frontmatter key it concerns, when it concerns one.
    pub field: Option<&'static str>,
    /// True when the finding makes the skill invalid rather than merely odd.
    pub is_error: bool,
    pub message: SharedString,
}

/// One skill, flattened from a [`DiscoveredSkill`] into what the panes need.
///
/// It carries its locations so that a delete plan can be recomputed from it
/// without holding the whole scan.
#[derive(Clone, Debug)]
pub struct SkillView {
    pub name: SharedString,
    pub description: SharedString,
    /// The real directory holding the bytes.
    pub origin: PathBuf,
    /// True when the origin is inside `~/.skillbase/store`.
    pub managed: bool,
    /// Why `SKILL.md` could not be parsed, when it could not.
    pub parse_error: Option<SharedString>,
    /// Validation problems, each tagged with the field that caused it.
    pub issues: Vec<Issue>,
    /// Other real directories claiming the same name.
    pub conflicts: Vec<PathBuf>,
    /// Every path this skill was found at.
    pub locations: Vec<Location>,
    /// True when `~/.codex/config.toml` switches this skill off.
    pub codex_disabled: bool,
    /// Ids of the agents that can currently reach this skill.
    pub visible_to: Vec<&'static str>,
    /// True when an active link sits in `~/.agents/skills`.
    pub in_shared: bool,
    /// Where the skill came from, when it says so. Read from its own
    /// frontmatter first and from the `npx skills` lockfile second; most
    /// skills were written by hand and carry neither, which is why this is an
    /// option rather than a default.
    pub provenance: Option<Provenance>,
}

impl SkillView {
    /// True when this agent holds a location of its own, active or parked in
    /// its disabled directory.
    ///
    /// This is *presence*, which for Claude Code and Codex is a different
    /// question from whether the skill is switched on.
    pub fn linked_to(&self, agent_id: &str) -> bool {
        self.locations.iter().any(|l| l.agent_id == agent_id)
    }

    /// What this agent's own location is on disk, when it has one.
    pub fn location_kind(&self, agent_id: &str) -> Option<&LocationKind> {
        self.locations
            .iter()
            .find(|l| l.agent_id == agent_id)
            .map(|l| &l.kind)
    }

    /// True when this agent's link is parked in its disabled directory.
    pub fn parked_in(&self, agent_id: &str) -> bool {
        self.locations
            .iter()
            .any(|l| l.agent_id == agent_id && matches!(l.kind, LocationKind::Disabled))
    }

    /// True when this agent reaches the skill only because it reads the shared
    /// directory, without a link of its own.
    pub fn via_shared(&self, agent: &AgentDef) -> bool {
        agent.reads_shared && self.in_shared && !self.linked_to(agent.id)
    }

    /// Whether the skill is switched on for an agent that draws a distinction
    /// between present and active.
    ///
    /// [`DisableMode::RemoveLink`] agents draw no such distinction, so for them
    /// this is the same question as presence and the interface does not offer
    /// a second switch.
    pub fn enabled_for(&self, agent: &AgentDef) -> bool {
        match agent.disable {
            DisableMode::RemoveLink => self.linked_to(agent.id) || self.via_shared(agent),
            DisableMode::MoveAside(_) => !self.parked_in(agent.id),
            DisableMode::CodexConfig => !self.codex_disabled,
        }
    }

    fn matches_query(&self, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        let query = query.to_lowercase();
        self.name.to_lowercase().contains(&query)
            || self.description.to_lowercase().contains(&query)
    }
}

/// True when an agent has a real "present but switched off" state, as opposed
/// to one where disabling just means removing the link.
pub fn has_disable_state(agent: &AgentDef) -> bool {
    !matches!(agent.disable, DisableMode::RemoveLink)
}

/// One scan of the filesystem, mapped for display.
///
/// Built on a background thread by [`Scan::load`] and then shared by reference;
/// nothing in it touches the filesystem again.
#[derive(Debug, Default)]
pub struct Scan {
    pub skills: Vec<SkillView>,
    /// Everything discovery could not read, in the order it was noticed.
    pub warnings: Vec<SharedString>,
    /// Agents whose global directory exists on this machine, in registry
    /// order, without the shared scope.
    pub installed: Vec<&'static AgentDef>,
    /// Every directory the scan looked in, and whether it is there: the store
    /// first, then the shared directory, then one row per agent.
    pub dirs: Vec<DirStatus>,
    /// How many skills each agent can see, by id.
    pub agent_counts: HashMap<&'static str, usize>,
    /// How many skills each Library row lists, in [`Library::ALL`] order.
    pub library_counts: [usize; Library::ALL.len()],
    /// Every skill that names where it came from, ready to be checked for an
    /// upstream update. Built here because it needs the whole discovery result
    /// and the `npx skills` lockfile, both of which are read on this thread.
    pub targets: Vec<UpdateTarget>,
}

impl Scan {
    /// Walks every scope under `roots` and maps the result.
    ///
    /// Blocking, and the only function in the application that reads the
    /// filesystem for a list of skills. Call it from a background task.
    pub fn load(roots: &Roots) -> Self {
        let result = Discovery::new(roots.clone()).run();

        let agent_counts: HashMap<&'static str, usize> =
            result.counts_by_agent().into_iter().collect();
        let installed = Registry::all()
            .iter()
            .filter(|agent| !agent.is_shared())
            .filter(|agent| roots.agent_dir(agent).is_dir())
            .collect::<Vec<_>>();

        // The store is `~/.agents/skills`, which the shared row below already
        // lists. What is worth a row of its own is the private directory,
        // because it is the one place a skill can be that no agent reads.
        let private = roots.private_dir();
        let mut dirs = vec![DirStatus {
            id: PRIVATE_ID,
            label: "Hidden skills".into(),
            exists: private.is_dir(),
            path: private,
        }];
        dirs.extend(Registry::all().iter().map(|agent| {
            let path = roots.agent_dir(agent);
            DirStatus {
                id: agent.id,
                label: agent.display_name.into(),
                exists: path.is_dir(),
                path,
            }
        }));

        // Read but never written: `npx skills` owns this file, and co-owning
        // it would invite two tools to race over it.
        let lock = SkillLock::read(roots);
        let targets = skillbase_core::update_targets(&result.skills, &lock);

        let skills: Vec<SkillView> = result
            .skills
            .iter()
            .map(|skill| SkillView::from_discovered(skill, &lock))
            .collect();

        let mut library_counts = [0usize; Library::ALL.len()];
        for library in Library::ALL {
            library_counts[library.index()] = skills.iter().filter(|s| library.matches(s)).count();
        }

        Self {
            skills,
            warnings: result.warnings.iter().map(SharedString::from).collect(),
            installed,
            dirs,
            agent_counts,
            library_counts,
            targets,
        }
    }

    /// The skill with this name, if the last scan found it.
    pub fn get(&self, name: &str) -> Option<&SkillView> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    /// How many skills a sidebar row lists.
    pub fn count(&self, scope: Scope) -> usize {
        match scope {
            Scope::Library(library) => self.library_counts[library.index()],
            Scope::Agent(id) => self.agent_counts.get(id).copied().unwrap_or(0),
        }
    }

    /// The skills a scope shows, narrowed by the search query.
    pub fn filter(&self, scope: Scope, query: &str) -> Vec<&SkillView> {
        self.skills
            .iter()
            .filter(|skill| scope.shows(skill) && skill.matches_query(query))
            .collect()
    }
}

impl SkillView {
    fn from_discovered(skill: &DiscoveredSkill, lock: &SkillLock) -> Self {
        let description = skill.description().unwrap_or_default();
        // A description is one line in the list, so a wrapped YAML scalar has
        // to lose its newlines before it gets there.
        let description: String = description.split_whitespace().collect::<Vec<_>>().join(" ");

        let issues = skill
            .doc
            .as_ref()
            .map(|doc| {
                doc.validate()
                    .into_iter()
                    .map(|issue| Issue {
                        field: issue.field,
                        is_error: issue.is_error(),
                        message: issue.error.to_string().into(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        Self {
            name: skill.name.clone().into(),
            description: description.into(),
            origin: skill.origin.clone(),
            managed: skill.managed,
            parse_error: skill
                .parse_error
                .as_ref()
                .map(|e| SharedString::from(e.to_string())),
            issues,
            conflicts: skill.conflicts.clone(),
            locations: skill.locations.clone(),
            codex_disabled: skill.codex_disabled,
            visible_to: skill.visible_to(),
            in_shared: skill.is_present_in(SHARED_ID),
            provenance: provenance_of(skill, lock),
        }
    }

    /// Rebuilds the discovery record a delete plan is computed from.
    ///
    /// [`skillbase_core::Installer::plan_delete`] reads only the origin and the
    /// locations, both of which this view carries, so a plan can be recomputed
    /// without keeping the whole scan alive.
    pub fn as_discovered(&self) -> DiscoveredSkill {
        DiscoveredSkill {
            name: self.name.to_string(),
            origin: self.origin.clone(),
            locations: self.locations.clone(),
            doc: None,
            parse_error: None,
            conflicts: self.conflicts.clone(),
            managed: self.managed,
            codex_disabled: self.codex_disabled,
        }
    }
}

/// Abbreviates a path under the home directory to `~/…`, so the interface can
/// name a location without a 90-character prefix.
pub fn display_path(path: &Path, roots: &Roots) -> SharedString {
    match path.strip_prefix(roots.home()) {
        Ok(rest) => format!("~/{}", rest.display()).into(),
        Err(_) => path.display().to_string().into(),
    }
}

/// The first seven characters of a git sha, which is how git itself shortens
/// one and enough to recognise it against a commit page.
pub fn short_sha(sha: &str) -> SharedString {
    let short: String = sha.chars().take(SHORT_SHA_LEN).collect();
    short.into()
}

/// How many characters of a sha the interface shows.
const SHORT_SHA_LEN: usize = 7;

/// Seconds since the Unix epoch, or 0 before it.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// How long ago a Unix timestamp was, in words.
///
/// Rounded down to the coarsest unit that still has a whole number in it,
/// because "checked 2 hours ago" is what the reader wants to know and "7412
/// seconds" is not. A timestamp in the future reads as "just now": a clock that
/// has been moved is not worth a sentence of its own.
pub fn ago(unix: i64) -> SharedString {
    if unix <= 0 {
        return "never".into();
    }
    let seconds = now_unix().saturating_sub(unix).max(0) as u64;
    // "just now" is already past tense. A counted duration is not, so it is the
    // one that needs the suffix: "checked 1 minute" is not English.
    match counted_duration(seconds) {
        Some(counted) => format!("{counted} ago").into(),
        None => "just now".into(),
    }
}

/// A number of seconds in words, for a wait the reader is being asked to sit
/// through. `zero` is what to say when there is nothing left to wait for.
pub fn in_words(seconds: u64) -> SharedString {
    match counted_duration(seconds) {
        Some(counted) => counted.into(),
        // Every caller reads "resets in {}", so the under-a-minute case has to
        // be a noun phrase that fits there. "any moment now" does not.
        None => "under a minute".into(),
    }
}

/// A duration as a count of the coarsest whole unit that fits, or `None` when
/// it is under a minute and there is no whole unit to name. The caller supplies
/// the wording for both cases, because "just now" and "any moment now" point in
/// opposite directions in time.
fn counted_duration(seconds: u64) -> Option<String> {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    let (count, unit) = match seconds {
        s if s < MINUTE => return None,
        s if s < HOUR => (s / MINUTE, "minute"),
        s if s < DAY => (s / HOUR, "hour"),
        s => (s / DAY, "day"),
    };
    Some(format!(
        "{count} {unit}{}",
        if count == 1 { "" } else { "s" }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duration_reads_as_the_coarsest_whole_unit() {
        assert_eq!(counted_duration(0), None);
        assert_eq!(counted_duration(59), None);
        assert_eq!(counted_duration(60).as_deref(), Some("1 minute"));
        assert_eq!(counted_duration(3599).as_deref(), Some("59 minutes"));
        assert_eq!(counted_duration(3600).as_deref(), Some("1 hour"));
        assert_eq!(counted_duration(86_400).as_deref(), Some("1 day"));
        assert_eq!(counted_duration(90_000).as_deref(), Some("1 day"));
    }

    #[test]
    fn a_timestamp_that_was_never_recorded_says_so() {
        assert_eq!(ago(0), "never");
        assert_eq!(ago(-1), "never");
    }

    #[test]
    fn a_past_time_reads_as_past_and_a_wait_does_not() {
        // The bug this guards: "checked 1 minute", with no "ago".
        assert_eq!(ago(now_unix() - 60), "1 minute ago");
        assert_eq!(ago(now_unix() - 7200), "2 hours ago");
        assert_eq!(ago(now_unix()), "just now");

        assert_eq!(in_words(60), "1 minute");
        assert_eq!(in_words(0), "under a minute");

        // Both callers read "resets in {}", so each phrasing must fit there.
        for seconds in [0, 30, 60, 7200] {
            let sentence = format!("It resets in {}.", in_words(seconds));
            assert!(!sentence.contains("in any"), "{sentence}");
        }
    }

    #[test]
    fn a_sha_is_shortened_the_way_git_shortens_one() {
        assert_eq!(
            short_sha("4f2a1c9d8e7b6a5f4e3d2c1b0a9f8e7d6c5b4a39"),
            "4f2a1c9"
        );
        assert_eq!(short_sha("abc"), "abc");
        assert_eq!(short_sha(""), "");
    }

    #[test]
    fn the_settings_reader_understands_what_the_writer_writes() {
        let written = format!("{{\n  \"show_all_agents\": {}\n}}\n", true);
        assert_eq!(json_bool(&written, "show_all_agents"), Some(true));
        let written = format!("{{\n  \"show_all_agents\": {}\n}}\n", false);
        assert_eq!(json_bool(&written, "show_all_agents"), Some(false));
    }

    #[test]
    fn a_missing_or_malformed_setting_reads_as_no_preference() {
        for text in [
            "",
            "{}",
            "not json at all",
            "{\"show_all_agents\": \"yes\"}",
            "{\"show_all_agents\":}",
            "{\"other\": true}",
        ] {
            assert_eq!(json_bool(text, "show_all_agents"), None, "{text:?}");
        }
    }

    #[test]
    fn whitespace_around_the_colon_does_not_matter() {
        assert_eq!(json_bool("{\"a\"   :   true}", "a"), Some(true));
        assert_eq!(json_bool("{\"a\":true}", "a"), Some(true));
    }
}
