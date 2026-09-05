//! The view model: one read-only scan of the filesystem, mapped into the shape
//! the three panes render.
//!
//! Everything here is plain data and pure functions. The scan itself is the one
//! function that touches the filesystem, and it is called from a background
//! task, never from `render`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui_kit::SharedString;
use skillbase_core::{
    AgentDef, DisableMode, DiscoveredSkill, Discovery, Location, LocationKind, Registry, Roots,
    SHARED_ID, SkillError,
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
            Library::Conflicts => "Conflicts",
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
    /// How many skills each agent can see, by id.
    pub agent_counts: HashMap<&'static str, usize>,
    /// How many skills each Library row lists, in [`Library::ALL`] order.
    pub library_counts: [usize; Library::ALL.len()],
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

        let skills: Vec<SkillView> = result
            .skills
            .iter()
            .map(SkillView::from_discovered)
            .collect();

        let mut library_counts = [0usize; Library::ALL.len()];
        for library in Library::ALL {
            library_counts[library.index()] = skills.iter().filter(|s| library.matches(s)).count();
        }

        Self {
            skills,
            warnings: result.warnings.iter().map(SharedString::from).collect(),
            installed,
            agent_counts,
            library_counts,
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
    fn from_discovered(skill: &DiscoveredSkill) -> Self {
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
