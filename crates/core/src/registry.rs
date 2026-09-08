//! The agent table: where each agent reads skills from, and how it is linked.
//!
//! This is deliberately *data*. [`Registry::all`] returns a static table that
//! can be extended without touching a single code path, because the list of
//! agents that read `~/.agents/skills/` changed substantially during 2026 and
//! will change again.
//!
//! Nothing here touches the filesystem. Paths are resolved against a home
//! directory handed in by the caller ([`Roots::new`]), which is what makes
//! discovery and installation testable against a fake home instead of the
//! user's real one. [`home_dir`] is the only function that asks the operating
//! system, and it is the application's job to call it.

use std::path::{Path, PathBuf};

use crate::error::SkillError;

/// The vendor-neutral shared directory, relative to the home directory.
pub const SHARED_SKILLS_DIR: &str = ".agents/skills";

/// Skillbase's store, relative to the home directory.
///
/// The same directory as [`SHARED_SKILLS_DIR`], named twice because it is used
/// for two things. As the store it is where Skillbase keeps the bytes of the
/// skills it owns; as the shared directory it is what fourteen agents read. A
/// skill whose origin lives here is *managed* and every agent that reads
/// `~/.agents/skills` already sees it, with no link in between.
pub const STORE_DIR: &str = SHARED_SKILLS_DIR;

/// Where Skillbase keeps the origin of a skill hidden from the agents, relative
/// to the home directory.
///
/// A managed skill whose Shared switch is off has its origin here instead of in
/// [`STORE_DIR`]. Nothing reads this directory except Skillbase, so the skill
/// is reachable only through the links agents hold to it.
pub const PRIVATE_DIR: &str = ".skillbase/private";

/// Where a deleted directory is moved to instead of being destroyed, relative
/// to the home directory.
///
/// Deleting a skill Skillbase did not download is deleting the only copy of
/// something a person wrote, so nothing under a managed scope is removed
/// recursively: it is renamed into here, under `<name>-<unix timestamp>`, and
/// the notification names the path it landed at. Skillbase never empties this
/// directory; the user does, in Finder, once they are sure.
pub const TRASH_DIR: &str = ".skillbase/trash";

/// The id of the shared scope, which is the `~/.agents/skills` directory itself
/// rather than an agent that reads it.
///
/// It is also the id a location in the store carries, because the store and the
/// shared directory are one directory.
pub const SHARED_ID: &str = "shared";

/// The id used for locations found in the private directory.
///
/// It is not an agent id; no [`AgentDef`] carries it.
pub const PRIVATE_ID: &str = "private";

/// How an agent's global skills directory is derived from the home directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GlobalDir {
    /// A path relative to the home directory, e.g. `.claude/skills`.
    UnderHome(&'static str),
    /// The shared directory itself, [`SHARED_SKILLS_DIR`].
    ///
    /// Two entries resolve this way: the `shared` scope and Zed, which reads
    /// `~/.agents/skills` as its own global directory. An agent that resolves
    /// here is never an independent link target — see
    /// [`AgentDef::covered_by_shared`].
    Shared,
}

impl GlobalDir {
    /// The path relative to the home directory.
    pub fn relative(self) -> &'static str {
        match self {
            Self::UnderHome(path) => path,
            Self::Shared => SHARED_SKILLS_DIR,
        }
    }
}

/// Whether a skill reaches an agent's directory as a symlink or as a copy.
///
/// Symlinks are the default and the point of the application: one edit lands
/// everywhere. `Copy` exists for an agent that is found not to resolve
/// symlinks; the table can select it without any other code changing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LinkMode {
    /// A relative symlink pointing at the origin directory.
    #[default]
    Symlink,
    /// A recursive copy of the origin, re-copied on every save.
    Copy,
}

/// How "present but switched off" is expressed for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DisableMode {
    /// The agent draws no distinction: disabling removes the link, and the UI
    /// must say so rather than implying a state that does not exist.
    RemoveLink,
    /// Move the link into a sibling directory, relative to the home directory.
    ///
    /// Claude Code uses `~/.claude/skills-disabled/`, the convention already in
    /// use on machines that do this.
    MoveAside(&'static str),
    /// Set `enabled = false` on the skill's `[[skills.config]]` entry in
    /// `~/.codex/config.toml`, leaving the directory where it is.
    CodexConfig,
}

/// One row of the agent table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentDef {
    /// Stable identifier, e.g. `claude-code`. Used as a key everywhere.
    pub id: &'static str,
    /// Name to show a human, e.g. `Claude Code`.
    pub display_name: &'static str,
    /// How to resolve the global skills directory.
    pub global_dir: GlobalDir,
    /// The project-scoped directory, relative to a project root. `None` when
    /// the agent has no project scope. Recorded for completeness; project
    /// scopes are out of scope for v1.
    pub project_dir: Option<&'static str>,
    /// Symlink or copy.
    pub link_mode: LinkMode,
    /// Whether the agent reads `~/.agents/skills` natively, so that a link in
    /// the shared directory is enough to make a skill visible to it.
    pub reads_shared: bool,
    /// How disabling works.
    pub disable: DisableMode,
}

impl AgentDef {
    /// The absolute global skills directory for this agent under `home`.
    pub fn resolve(&self, home: &Path) -> PathBuf {
        home.join(self.global_dir.relative())
    }

    /// True for the shared scope itself.
    pub fn is_shared(&self) -> bool {
        self.id == SHARED_ID
    }

    /// True when this agent's global directory *is* the shared directory, so it
    /// cannot be linked independently.
    ///
    /// Zed is the case: linking "into Zed" would mean writing into
    /// `~/.agents/skills`, which is the shared switch. Such an agent is
    /// presented as covered by Shared, never as a target of its own.
    pub fn covered_by_shared(&self) -> bool {
        matches!(self.global_dir, GlobalDir::Shared) && !self.is_shared()
    }

    /// True when a link may be created in this agent's own directory.
    pub fn is_link_target(&self) -> bool {
        !self.covered_by_shared()
    }

    /// The directory disabled skills are moved to, when disabling works that
    /// way. Relative to the home directory.
    pub fn disabled_dir_relative(&self) -> Option<&'static str> {
        match self.disable {
            DisableMode::MoveAside(dir) => Some(dir),
            _ => None,
        }
    }
}

/// The agent table.
///
/// Order matters: it is the display order in the sidebar and the tie-break when
/// two real directories claim the same skill name.
static AGENTS: &[AgentDef] = &[
    AgentDef {
        id: SHARED_ID,
        display_name: "All agents (shared)",
        global_dir: GlobalDir::Shared,
        project_dir: Some(".agents/skills"),
        link_mode: LinkMode::Symlink,
        reads_shared: true,
        disable: DisableMode::RemoveLink,
    },
    AgentDef {
        id: "claude-code",
        display_name: "Claude Code",
        global_dir: GlobalDir::UnderHome(".claude/skills"),
        project_dir: Some(".claude/skills"),
        link_mode: LinkMode::Symlink,
        reads_shared: false,
        disable: DisableMode::MoveAside(".claude/skills-disabled"),
    },
    AgentDef {
        id: "codex",
        display_name: "Codex",
        global_dir: GlobalDir::UnderHome(".codex/skills"),
        project_dir: Some(".agents/skills"),
        link_mode: LinkMode::Symlink,
        reads_shared: true,
        disable: DisableMode::CodexConfig,
    },
    AgentDef {
        id: "cursor",
        display_name: "Cursor",
        global_dir: GlobalDir::UnderHome(".cursor/skills"),
        project_dir: Some(".cursor/skills"),
        link_mode: LinkMode::Symlink,
        reads_shared: true,
        disable: DisableMode::RemoveLink,
    },
    AgentDef {
        id: "gemini-cli",
        display_name: "Gemini CLI",
        global_dir: GlobalDir::UnderHome(".gemini/skills"),
        project_dir: Some(".gemini/skills"),
        link_mode: LinkMode::Symlink,
        reads_shared: true,
        disable: DisableMode::RemoveLink,
    },
    AgentDef {
        id: "opencode",
        display_name: "opencode",
        global_dir: GlobalDir::UnderHome(".config/opencode/skills"),
        project_dir: Some(".opencode/skills"),
        link_mode: LinkMode::Symlink,
        reads_shared: true,
        disable: DisableMode::RemoveLink,
    },
    AgentDef {
        id: "goose",
        display_name: "Goose",
        global_dir: GlobalDir::UnderHome(".config/goose/skills"),
        project_dir: Some(".goose/skills"),
        link_mode: LinkMode::Symlink,
        reads_shared: true,
        disable: DisableMode::RemoveLink,
    },
    AgentDef {
        id: "amp",
        display_name: "Amp",
        global_dir: GlobalDir::UnderHome(".config/agents/skills"),
        project_dir: Some(".agents/skills"),
        link_mode: LinkMode::Symlink,
        reads_shared: true,
        disable: DisableMode::RemoveLink,
    },
    AgentDef {
        id: "copilot",
        display_name: "GitHub Copilot",
        global_dir: GlobalDir::UnderHome(".copilot/skills"),
        project_dir: Some(".github/skills"),
        link_mode: LinkMode::Symlink,
        reads_shared: true,
        disable: DisableMode::RemoveLink,
    },
    AgentDef {
        id: "zed",
        display_name: "Zed",
        global_dir: GlobalDir::Shared,
        project_dir: Some(".agents/skills"),
        link_mode: LinkMode::Symlink,
        // Zed reads the shared directory, and its global directory *is* the
        // shared directory. Project scope applies to trusted worktrees only.
        reads_shared: true,
        disable: DisableMode::RemoveLink,
    },
    AgentDef {
        id: "cline",
        display_name: "Cline",
        global_dir: GlobalDir::UnderHome(".cline/skills"),
        project_dir: Some(".cline/skills"),
        link_mode: LinkMode::Symlink,
        reads_shared: false,
        disable: DisableMode::RemoveLink,
    },
    AgentDef {
        id: "junie",
        display_name: "JetBrains Junie",
        global_dir: GlobalDir::UnderHome(".junie/skills"),
        project_dir: Some(".junie/skills"),
        link_mode: LinkMode::Symlink,
        reads_shared: true,
        disable: DisableMode::RemoveLink,
    },
    AgentDef {
        id: "warp",
        display_name: "Warp",
        global_dir: GlobalDir::UnderHome(".warp/skills"),
        project_dir: Some(".warp/skills"),
        link_mode: LinkMode::Symlink,
        reads_shared: true,
        disable: DisableMode::RemoveLink,
    },
    AgentDef {
        id: "kiro",
        display_name: "Kiro",
        global_dir: GlobalDir::UnderHome(".kiro/skills"),
        project_dir: Some(".kiro/skills"),
        link_mode: LinkMode::Symlink,
        reads_shared: true,
        disable: DisableMode::RemoveLink,
    },
    AgentDef {
        id: "devin",
        display_name: "Devin Desktop",
        global_dir: GlobalDir::UnderHome(".devin/skills"),
        project_dir: Some(".devin/skills"),
        link_mode: LinkMode::Symlink,
        reads_shared: true,
        disable: DisableMode::RemoveLink,
    },
];

/// Agents Skillbase deliberately does not support, and why.
///
/// Naming them in the UI is better than silently omitting them: a user who
/// wonders where Aider is deserves an answer.
pub static UNSUPPORTED: &[(&str, &str)] = &[
    (
        "Aider",
        "Reads flat prose convention files, not skill directories. \
         Symlinking a skill directory there produces nothing useful.",
    ),
    (
        "Continue.dev",
        "Archived in 2026 without ever supporting the skill format.",
    ),
    (
        "Claude Desktop",
        "Stores skills server-side as uploaded archives. It has no filesystem \
         presence, so it is out of reach by construction.",
    ),
];

/// Lookup over the agent table.
///
/// The table is static, so this type holds nothing; it exists to give the
/// lookups a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Registry;

impl Registry {
    /// Every agent, in display order. The shared scope is the first entry.
    pub fn all() -> &'static [AgentDef] {
        AGENTS
    }

    /// The agent with this id, if the table has one.
    pub fn get(id: &str) -> Option<&'static AgentDef> {
        AGENTS.iter().find(|agent| agent.id == id)
    }

    /// The shared scope.
    pub fn shared() -> &'static AgentDef {
        &AGENTS[0]
    }

    /// Agents that may hold a link of their own, i.e. everything but those
    /// whose global directory is the shared directory.
    pub fn link_targets() -> impl Iterator<Item = &'static AgentDef> {
        AGENTS.iter().filter(|agent| agent.is_link_target())
    }

    /// Agents covered by a link in the shared directory, in display order.
    ///
    /// Used to label the Shared switch with what it actually reaches.
    pub fn covered_by_shared() -> impl Iterator<Item = &'static AgentDef> {
        AGENTS
            .iter()
            .filter(|agent| agent.reads_shared && !agent.is_shared())
    }

    /// Position of an id in the table, used as a stable tie-break.
    pub fn order_of(id: &str) -> usize {
        AGENTS
            .iter()
            .position(|agent| agent.id == id)
            .unwrap_or(usize::MAX)
    }
}

/// The user's home directory, or an error naming why it could not be found.
///
/// The only place this crate asks the operating system anything about the
/// environment. Everything else takes a home directory as a parameter.
pub fn home_dir() -> Result<PathBuf, SkillError> {
    dirs::home_dir().ok_or_else(|| {
        SkillError::io(
            PathBuf::from("~"),
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "could not determine the home directory",
            ),
        )
    })
}

/// Every directory Skillbase reads or writes, resolved against one home.
///
/// Discovery and installation take a `Roots` rather than calling
/// [`home_dir`] themselves, so tests point them at a temporary directory and
/// can never touch the real home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roots {
    home: PathBuf,
}

impl Roots {
    /// Resolves every path against `home`.
    ///
    /// The home path is canonicalized when it exists, so that paths derived
    /// from it compare equal to paths that came back from
    /// [`std::fs::canonicalize`] — on macOS a temporary directory is reached
    /// through `/var`, which is a symlink to `/private/var`.
    pub fn new(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let home = std::fs::canonicalize(&home).unwrap_or(home);
        Self { home }
    }

    /// Resolves against the real home directory.
    pub fn discover() -> Result<Self, SkillError> {
        Ok(Self::new(home_dir()?))
    }

    /// The home directory every other path hangs off.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// `~/.agents/skills`, the vendor-neutral directory agents read.
    ///
    /// The same directory as [`Roots::store_dir`]. Call it this way when the
    /// question is which agents can reach a skill.
    pub fn shared_dir(&self) -> PathBuf {
        self.home.join(SHARED_SKILLS_DIR)
    }

    /// `~/.agents/skills`, the directory Skillbase keeps the bytes in.
    ///
    /// The same directory as [`Roots::shared_dir`]. Call it this way when the
    /// question is whether Skillbase owns the origin and may move it.
    pub fn store_dir(&self) -> PathBuf {
        self.home.join(STORE_DIR)
    }

    /// `~/.skillbase/private`, where the origin of a hidden skill sits.
    ///
    /// Skillbase owns this directory too, so a skill whose origin is here is
    /// managed; it is simply out of the way of the agents that read
    /// [`Roots::shared_dir`].
    pub fn private_dir(&self) -> PathBuf {
        self.home.join(PRIVATE_DIR)
    }

    /// `~/.skillbase/trash`, where a deleted directory is moved to.
    ///
    /// In [`Roots::scope_roots`] so a delete is allowed to write into it, and
    /// out of [`Roots::managed_roots`] so nothing that lands here counts as a
    /// skill the user has.
    pub fn trash_dir(&self) -> PathBuf {
        self.home.join(TRASH_DIR)
    }

    /// `~/.codex/config.toml`.
    pub fn codex_config(&self) -> PathBuf {
        self.home.join(".codex/config.toml")
    }

    /// The global skills directory for one agent.
    pub fn agent_dir(&self, agent: &AgentDef) -> PathBuf {
        agent.resolve(&self.home)
    }

    /// The directory this agent's disabled links are parked in, when it has
    /// one.
    pub fn disabled_dir(&self, agent: &AgentDef) -> Option<PathBuf> {
        agent.disabled_dir_relative().map(|dir| self.home.join(dir))
    }

    /// Every directory a destructive operation is allowed to touch: the store,
    /// the private directory, the trash, each agent's global directory, and
    /// each disabled directory.
    ///
    /// Deduplicated and in registry order, with the store first. The store is
    /// also the shared scope's directory, so the shared entry adds nothing and
    /// is dropped by the deduplication.
    ///
    /// This is where writes are allowed, which is not the same question as
    /// what Skillbase has: the trash is in this list because a delete has to
    /// be able to write into it, and out of [`Roots::managed_roots`] because
    /// nothing reads it. Ask this one before writing; ask the other before
    /// deciding whether a directory is a skill the user already has.
    pub fn scope_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.store_dir(), self.private_dir(), self.trash_dir()];
        for agent in Registry::all() {
            let dir = self.agent_dir(agent);
            if !roots.contains(&dir) {
                roots.push(dir);
            }
            if let Some(disabled) = self.disabled_dir(agent)
                && !roots.contains(&disabled)
            {
                roots.push(disabled);
            }
        }
        roots
    }

    /// Every directory whose content Skillbase manages: [`Roots::scope_roots`]
    /// without the trash.
    ///
    /// A skill directory in one of these is one the user has, and discovery
    /// finds it. A directory in the trash is neither: discovery never scans
    /// the trash, so a deleted skill is not in the list, nothing can be
    /// adopted out of it, and importing a folder that sits in it is an import
    /// like any other.
    pub fn managed_roots(&self) -> Vec<PathBuf> {
        let trash = self.trash_dir();
        self.scope_roots()
            .into_iter()
            .filter(|root| *root != trash)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_holds_every_agent_exactly_once() {
        let ids: Vec<_> = Registry::all().iter().map(|a| a.id).collect();
        assert_eq!(
            ids,
            [
                "shared",
                "claude-code",
                "codex",
                "cursor",
                "gemini-cli",
                "opencode",
                "goose",
                "amp",
                "copilot",
                "zed",
                "cline",
                "junie",
                "warp",
                "kiro",
                "devin",
            ]
        );
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "ids must be unique");
    }

    #[test]
    fn global_directories_resolve_under_the_given_home() {
        let home = Path::new("/fake/home");
        let dir = |id: &str| Registry::get(id).unwrap().resolve(home);
        assert_eq!(dir("shared"), Path::new("/fake/home/.agents/skills"));
        assert_eq!(dir("claude-code"), Path::new("/fake/home/.claude/skills"));
        assert_eq!(
            dir("opencode"),
            Path::new("/fake/home/.config/opencode/skills")
        );
        assert_eq!(dir("amp"), Path::new("/fake/home/.config/agents/skills"));
        assert_eq!(dir("copilot"), Path::new("/fake/home/.copilot/skills"));
        assert_eq!(
            dir("zed"),
            dir("shared"),
            "Zed's global directory is the shared directory"
        );
    }

    #[test]
    fn zed_is_covered_by_shared_and_is_not_a_link_target() {
        let zed = Registry::get("zed").unwrap();
        assert!(zed.covered_by_shared());
        assert!(!zed.is_link_target());
        assert!(Registry::shared().is_link_target());
        assert!(!Registry::shared().covered_by_shared());
        let targets: Vec<_> = Registry::link_targets().map(|a| a.id).collect();
        assert!(targets.contains(&"shared"));
        assert!(!targets.contains(&"zed"));
        assert_eq!(targets.len(), Registry::all().len() - 1);
    }

    #[test]
    fn claude_code_and_cline_do_not_read_the_shared_directory() {
        for id in ["claude-code", "cline"] {
            assert!(!Registry::get(id).unwrap().reads_shared, "{id}");
        }
        let covered: Vec<_> = Registry::covered_by_shared().map(|a| a.id).collect();
        assert!(!covered.contains(&"claude-code"));
        assert!(!covered.contains(&"cline"));
        assert!(covered.contains(&"zed"));
        assert!(covered.contains(&"codex"));
        assert_eq!(covered.len(), Registry::all().len() - 3);
    }

    #[test]
    fn disable_modes_match_the_specification() {
        assert_eq!(
            Registry::get("claude-code").unwrap().disable,
            DisableMode::MoveAside(".claude/skills-disabled")
        );
        assert_eq!(
            Registry::get("codex").unwrap().disable,
            DisableMode::CodexConfig
        );
        for agent in Registry::all() {
            if !matches!(agent.id, "claude-code" | "codex") {
                assert_eq!(agent.disable, DisableMode::RemoveLink, "{}", agent.id);
            }
            assert_eq!(agent.link_mode, LinkMode::Symlink, "{}", agent.id);
        }
    }

    #[test]
    fn the_store_is_the_shared_directory_and_private_sits_beside_it() {
        let roots = Roots::new("/fake/home");
        assert_eq!(roots.store_dir(), roots.shared_dir());
        assert_eq!(roots.store_dir(), Path::new("/fake/home/.agents/skills"));
        assert_eq!(
            roots.private_dir(),
            Path::new("/fake/home/.skillbase/private")
        );
        assert_eq!(roots.trash_dir(), Path::new("/fake/home/.skillbase/trash"));
    }

    #[test]
    fn scope_roots_are_deduplicated_and_include_the_store_and_disabled_dirs() {
        let roots = Roots::new("/fake/home");
        let paths = roots.scope_roots();
        assert_eq!(paths[0], Path::new("/fake/home/.agents/skills"));
        assert_eq!(paths[1], Path::new("/fake/home/.skillbase/private"));
        assert_eq!(
            paths[2],
            Path::new("/fake/home/.skillbase/trash"),
            "a delete has to be allowed to write into the trash"
        );
        assert!(paths.contains(&PathBuf::from("/fake/home/.claude/skills-disabled")));
        let shared = PathBuf::from("/fake/home/.agents/skills");
        assert_eq!(
            paths.iter().filter(|p| **p == shared).count(),
            1,
            "the store, the shared scope and Zed are one directory, listed once"
        );
        let mut sorted = paths.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), paths.len());
    }

    #[test]
    fn managed_roots_are_the_scope_roots_without_the_trash() {
        let roots = Roots::new("/fake/home");
        let managed = roots.managed_roots();
        assert!(
            !managed.contains(&roots.trash_dir()),
            "a directory in the trash is not a skill Skillbase has: {managed:?}"
        );
        assert_eq!(managed[0], Path::new("/fake/home/.agents/skills"));
        assert_eq!(managed[1], Path::new("/fake/home/.skillbase/private"));
        assert!(managed.contains(&PathBuf::from("/fake/home/.claude/skills")));
        assert!(managed.contains(&PathBuf::from("/fake/home/.claude/skills-disabled")));
        assert_eq!(
            managed.len(),
            roots.scope_roots().len() - 1,
            "the trash is the only difference"
        );
    }

    #[test]
    fn unsupported_agents_carry_a_reason() {
        let names: Vec<_> = UNSUPPORTED.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["Aider", "Continue.dev", "Claude Desktop"]);
        for (name, reason) in UNSUPPORTED {
            assert!(!reason.is_empty(), "{name} needs a reason");
            assert!(Registry::get(name).is_none());
        }
    }
}
