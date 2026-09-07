//! Read-only scan of every scope on the machine.
//!
//! Discovery walks the store — which is the shared directory `~/.agents/skills`
//! — the private directory and each agent's global directory, groups what it
//! finds by skill name, and works out which path holds the real bytes
//! ([`LocationKind::Origin`]) and which are links to it.
//!
//! **Discovery never writes.** It opens directories, reads `SKILL.md` files and
//! resolves symlinks, and does nothing else. A test asserts the fixture tree is
//! byte-for-byte unchanged after a scan.
//!
//! Nothing in a scan can panic. Unreadable directories, broken symlinks,
//! symlink loops, a `SKILL.md` that is a directory and permission failures all
//! land in [`DiscoveryResult::warnings`] and the scan carries on.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::doc::SkillDoc;
use crate::error::SkillError;
use crate::registry::{AgentDef, PRIVATE_ID, Registry, Roots, SHARED_ID};
use crate::skill::SKILL_FILE_NAME;

/// What a path holding a skill actually is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocationKind {
    /// A real directory holding the bytes. Exactly one location per skill is
    /// the origin.
    Origin,
    /// A symlink pointing at the origin, or at something outside every scope.
    Symlink {
        /// Where the link resolves to, canonicalized.
        target: PathBuf,
    },
    /// A real directory that duplicates the origin rather than linking to it.
    ///
    /// A copy goes stale the moment the skill is edited. Skillbase reports it
    /// rather than pretending it is the same skill.
    Copy,
    /// A link parked in the agent's disabled directory: present, switched off.
    Disabled,
}

impl LocationKind {
    /// True when this location makes the skill reachable by its agent.
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::Disabled)
    }
}

/// One place a skill was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// The scope this path belongs to: an agent id, or [`PRIVATE_ID`].
    ///
    /// A path in the store carries [`SHARED_ID`], because the store is the
    /// shared directory.
    pub agent_id: &'static str,
    /// The skill directory as it was found, before following any symlink.
    pub path: PathBuf,
    /// What the path is.
    pub kind: LocationKind,
}

/// One skill, with every place it was found.
#[derive(Debug)]
pub struct DiscoveredSkill {
    /// The name agents see: the frontmatter `name` when it parsed, otherwise
    /// the directory name.
    pub name: String,
    /// The real directory holding the bytes, never a symlink.
    pub origin: PathBuf,
    /// Every path this skill was found at, in registry order.
    pub locations: Vec<Location>,
    /// The parsed `SKILL.md`, or `None` when it could not be parsed.
    pub doc: Option<SkillDoc>,
    /// Why the `SKILL.md` could not be parsed, when it could not.
    pub parse_error: Option<SkillError>,
    /// Other real directories claiming the same name.
    ///
    /// Non-empty means the machine has more than one copy of this skill and
    /// editing one will not change the others. Reported, never hidden.
    pub conflicts: Vec<PathBuf>,
    /// True when the origin is inside `~/.agents/skills` or
    /// `~/.skillbase/private`, so Skillbase owns it and may add or remove links
    /// freely.
    ///
    /// Those are the two directories Skillbase writes origins to: the store,
    /// where the agents that read `~/.agents/skills` find the skill, and the
    /// private directory, where the origin sits when the user has hidden it
    /// from them. An origin anywhere else — `~/.claude/skills/foo`, say —
    /// belongs to another tool, so the skill is unmanaged and its visibility is
    /// reported read-only.
    pub managed: bool,
    /// True when `~/.codex/config.toml` carries an `enabled = false` entry for
    /// this skill.
    pub codex_disabled: bool,
}

impl DiscoveredSkill {
    /// The frontmatter description, when the document parsed and has one.
    pub fn description(&self) -> Option<&str> {
        self.doc.as_ref()?.frontmatter.description()
    }

    /// True when more than one real directory claims this name.
    pub fn has_conflict(&self) -> bool {
        !self.conflicts.is_empty()
    }

    /// The location for one scope id, if there is one.
    pub fn location(&self, agent_id: &str) -> Option<&Location> {
        self.locations.iter().find(|l| l.agent_id == agent_id)
    }

    /// True when an active (not disabled) location exists in that scope.
    pub fn is_present_in(&self, agent_id: &str) -> bool {
        self.locations
            .iter()
            .any(|l| l.agent_id == agent_id && l.kind.is_active())
    }

    /// The agent ids that can currently reach this skill, in registry order.
    ///
    /// An agent reaches a skill when it has an active location of its own, or
    /// when it reads the shared directory and the skill is active there. Codex
    /// is excluded when its config disables the skill, whatever the filesystem
    /// says.
    ///
    /// The shared scope is a directory, not an agent, so it never appears here;
    /// ask [`DiscoveredSkill::is_present_in`] with [`SHARED_ID`] for that.
    ///
    /// [`SHARED_ID`]: crate::registry::SHARED_ID
    pub fn visible_to(&self) -> Vec<&'static str> {
        let in_shared = self.is_present_in(crate::registry::SHARED_ID);
        Registry::all()
            .iter()
            .filter(|agent| !agent.is_shared())
            .filter(|agent| {
                if agent.id == "codex" && self.codex_disabled {
                    return false;
                }
                self.is_present_in(agent.id) || (agent.reads_shared && in_shared)
            })
            .map(|agent| agent.id)
            .collect()
    }
}

/// The outcome of one scan.
#[derive(Debug, Default)]
pub struct DiscoveryResult {
    /// Every skill found, sorted by name.
    pub skills: Vec<DiscoveredSkill>,
    /// Everything that went wrong without stopping the scan, in the order it
    /// was noticed.
    pub warnings: Vec<String>,
}

impl DiscoveryResult {
    /// The skill with this name, if it was found.
    pub fn get(&self, name: &str) -> Option<&DiscoveredSkill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// How many skills each agent can currently see, in registry order.
    pub fn counts_by_agent(&self) -> Vec<(&'static str, usize)> {
        Registry::all()
            .iter()
            .map(|agent| {
                let count = if agent.is_shared() {
                    self.skills
                        .iter()
                        .filter(|s| s.is_present_in(agent.id))
                        .count()
                } else {
                    self.skills
                        .iter()
                        .filter(|s| s.visible_to().contains(&agent.id))
                        .count()
                };
                (agent.id, count)
            })
            .collect()
    }
}

/// A read-only scan of one home directory.
#[derive(Debug, Clone)]
pub struct Discovery {
    roots: Roots,
}

impl Discovery {
    /// Scans the scopes under `roots`.
    pub fn new(roots: Roots) -> Self {
        Self { roots }
    }

    /// The roots this scan runs against.
    pub fn roots(&self) -> &Roots {
        &self.roots
    }

    /// Walks every scope and groups what it finds by skill name.
    ///
    /// Never fails: anything unreadable becomes a warning.
    pub fn run(&self) -> DiscoveryResult {
        let mut warnings = Vec::new();
        let mut docs = DocCache::default();
        let mut candidates = Vec::new();

        for scope in self.scopes() {
            self.scan_dir(&scope, &mut candidates, &mut docs, &mut warnings);
        }

        let codex_disabled = self.codex_disabled_dirs(&mut warnings);
        let groups = group_by_name(candidates, &docs);
        let store = self.roots.store_dir();
        let private = self.roots.private_dir();

        let mut skills: Vec<_> = groups
            .into_values()
            .map(|group| build_skill(group, &mut docs, &store, &private, &codex_disabled))
            .collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));

        DiscoveryResult { skills, warnings }
    }

    /// The directories to walk, deduplicated by path, in registry order with
    /// the store first and the private directory second.
    ///
    /// The store is the shared directory, so it is walked once, under
    /// [`SHARED_ID`]; the shared entry in the agent table adds nothing and is
    /// dropped by the deduplication. Zed's global directory is that same
    /// directory, so it is not walked either, and Zed's visibility comes from
    /// `reads_shared`.
    fn scopes(&self) -> Vec<Scope> {
        let mut scopes = vec![
            Scope {
                agent_id: SHARED_ID,
                path: self.roots.store_dir(),
                disabled: false,
            },
            Scope {
                agent_id: PRIVATE_ID,
                path: self.roots.private_dir(),
                disabled: false,
            },
        ];
        for agent in Registry::all() {
            let path = self.roots.agent_dir(agent);
            if !scopes.iter().any(|s| s.path == path) {
                scopes.push(Scope {
                    agent_id: agent.id,
                    path,
                    disabled: false,
                });
            }
            if let Some(path) = self.roots.disabled_dir(agent)
                && !scopes.iter().any(|s| s.path == path)
            {
                scopes.push(Scope {
                    agent_id: agent.id,
                    path,
                    disabled: true,
                });
            }
        }
        scopes
    }

    /// Lists one scope directory. Every failure becomes a warning.
    fn scan_dir(
        &self,
        scope: &Scope,
        out: &mut Vec<Candidate>,
        docs: &mut DocCache,
        warnings: &mut Vec<String>,
    ) {
        let entries = match fs::read_dir(&scope.path) {
            Ok(entries) => entries,
            // A scope directory that does not exist means the agent is not
            // installed. That is the common case, not a problem.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                warnings.push(format!("cannot read {}: {e}", scope.path.display()));
                return;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    warnings.push(format!(
                        "cannot read an entry in {}: {e}",
                        scope.path.display()
                    ));
                    continue;
                }
            };
            let path = entry.path();

            let link_meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(e) => {
                    warnings.push(format!("cannot stat {}: {e}", path.display()));
                    continue;
                }
            };
            let is_symlink = link_meta.file_type().is_symlink();

            // Resolving a symlink is where a broken link or a loop shows up.
            let target_meta = match fs::metadata(&path) {
                Ok(meta) => meta,
                Err(e) if is_symlink => {
                    warnings.push(format!("broken symlink {}: {e}", path.display()));
                    continue;
                }
                Err(e) => {
                    warnings.push(format!("cannot read {}: {e}", path.display()));
                    continue;
                }
            };
            if !target_meta.is_dir() {
                // A stray file such as .DS_Store. Not a skill, not a problem.
                continue;
            }

            match fs::symlink_metadata(path.join(SKILL_FILE_NAME)) {
                Ok(meta) if meta.file_type().is_dir() => {
                    warnings.push(format!(
                        "{}: {SKILL_FILE_NAME} is a directory, not a file",
                        path.display()
                    ));
                    continue;
                }
                // A directory without a SKILL.md is not a skill. Skipped in
                // silence, because agent directories hold plenty of them.
                Err(_) => continue,
                Ok(_) => {}
            }

            let real = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            docs.load(&real, warnings);
            out.push(Candidate {
                agent_id: scope.agent_id,
                path,
                real,
                is_symlink,
                disabled: scope.disabled,
                dir_name: entry.file_name().to_string_lossy().into_owned(),
            });
        }
    }

    /// The skill directories Codex has switched off in its config file.
    fn codex_disabled_dirs(&self, warnings: &mut Vec<String>) -> HashSet<PathBuf> {
        let config = self.roots.codex_config();
        match codex_disabled_paths(&config) {
            Ok(paths) => paths
                .into_iter()
                .map(|p| fs::canonicalize(&p).unwrap_or(p))
                .collect(),
            Err(e) => {
                warnings.push(format!("cannot read {}: {e}", config.display()));
                HashSet::new()
            }
        }
    }
}

/// Reads the skill directories disabled in a Codex `config.toml`.
///
/// Codex records one `[[skills.config]]` table per skill, keyed by the path of
/// its `SKILL.md`; `enabled = false` switches it off without moving anything.
/// The returned paths are the skill *directories*, i.e. the parent of each
/// disabled `SKILL.md`.
///
/// A missing config file is not an error and yields an empty list. This
/// function only reads.
pub fn codex_disabled_paths(config: &Path) -> Result<Vec<PathBuf>, SkillError> {
    let text = match fs::read_to_string(config) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(SkillError::io(config, e)),
    };
    let doc = text.parse::<toml_edit::DocumentMut>().map_err(|e| {
        SkillError::io(
            config,
            std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
        )
    })?;

    let Some(entries) = doc
        .get("skills")
        .and_then(|skills| skills.get("config"))
        .and_then(|config| config.as_array_of_tables())
    else {
        return Ok(Vec::new());
    };

    Ok(entries
        .iter()
        .filter(|entry| entry.get("enabled").and_then(|v| v.as_bool()) == Some(false))
        .filter_map(|entry| entry.get("path")?.as_str())
        .map(|path| {
            let path = Path::new(path);
            // The entry names SKILL.md; the skill is its directory.
            match path.file_name() {
                Some(name) if name == SKILL_FILE_NAME => path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| path.to_path_buf()),
                _ => path.to_path_buf(),
            }
        })
        .collect())
}

/// Sort key that puts the store first, the private directory second, and then
/// follows the agent table, so a skill's locations come back in the order the
/// interface lists them.
///
/// The store carries [`SHARED_ID`], which is also the agent table's first row,
/// so it would sort first anyway; naming it here keeps the private directory
/// from colliding with Claude Code.
fn scope_order(agent_id: &str) -> usize {
    match agent_id {
        SHARED_ID => 0,
        PRIVATE_ID => 1,
        _ => Registry::order_of(agent_id).saturating_add(1),
    }
}

/// One directory to walk.
struct Scope {
    agent_id: &'static str,
    path: PathBuf,
    disabled: bool,
}

/// One skill directory found in one scope, before grouping.
struct Candidate {
    agent_id: &'static str,
    /// The path as found, symlink and all.
    path: PathBuf,
    /// The path with symlinks resolved. Two candidates with the same `real` are
    /// the same bytes.
    real: PathBuf,
    is_symlink: bool,
    disabled: bool,
    dir_name: String,
}

/// Parsed `SKILL.md` files, keyed by canonical directory.
///
/// Every scope holding the same skill points at one directory, so without this
/// a fan-out of fifteen agents would re-read and re-parse the same file fifteen
/// times.
#[derive(Default)]
struct DocCache {
    docs: HashMap<PathBuf, Result<SkillDoc, SkillError>>,
}

impl DocCache {
    fn load(&mut self, real: &Path, warnings: &mut Vec<String>) {
        if self.docs.contains_key(real) {
            return;
        }
        let file = real.join(SKILL_FILE_NAME);
        let parsed = fs::read_to_string(&file)
            .map_err(|e| SkillError::io(&file, e))
            .and_then(|text| SkillDoc::parse(&text));
        if let Err(e) = &parsed {
            warnings.push(format!("{}: {e}", file.display()));
        }
        self.docs.insert(real.to_path_buf(), parsed);
    }

    fn name(&self, real: &Path) -> Option<&str> {
        match self.docs.get(real) {
            Some(Ok(doc)) => doc.frontmatter.name().filter(|n| !n.is_empty()),
            _ => None,
        }
    }

    fn take(&mut self, real: &Path) -> (Option<SkillDoc>, Option<SkillError>) {
        match self.docs.remove(real) {
            Some(Ok(doc)) => (Some(doc), None),
            Some(Err(e)) => (None, Some(e)),
            None => (None, None),
        }
    }
}

/// Groups candidates by the name agents will see.
///
/// The frontmatter `name` wins over the directory name, because that is what an
/// agent loads the skill by. A `BTreeMap` keeps the grouping deterministic.
fn group_by_name(candidates: Vec<Candidate>, docs: &DocCache) -> BTreeMap<String, Vec<Candidate>> {
    let mut groups: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
    for candidate in candidates {
        let name = docs
            .name(&candidate.real)
            .unwrap_or(&candidate.dir_name)
            .to_string();
        groups.entry(name).or_default().push(candidate);
    }
    groups
}

/// Turns one group of candidates into a skill, deciding which path is the
/// origin.
fn build_skill(
    mut group: Vec<Candidate>,
    docs: &mut DocCache,
    store: &Path,
    private: &Path,
    codex_disabled: &HashSet<PathBuf>,
) -> DiscoveredSkill {
    group.sort_by_key(|c| (scope_order(c.agent_id), c.path.clone()));

    // Real directories are origin candidates. Symlinks point at one.
    let reals: Vec<PathBuf> = group
        .iter()
        .filter(|c| !c.is_symlink)
        .map(|c| c.real.clone())
        .collect();

    // Preference: the store, then the private directory, then registry order —
    // which `group` is already sorted by.
    let origin = reals
        .iter()
        .find(|p| p.starts_with(store))
        .or_else(|| reals.iter().find(|p| p.starts_with(private)))
        .or_else(|| reals.first())
        // Every location is a symlink out of every scanned scope: the origin is
        // whatever they resolve to, even though it was never walked.
        .or_else(|| group.first().map(|c| &c.real))
        .cloned()
        .unwrap_or_default();

    let mut conflicts: Vec<PathBuf> = reals.into_iter().filter(|p| *p != origin).collect();
    conflicts.dedup();

    let locations = group
        .iter()
        .map(|c| Location {
            agent_id: c.agent_id,
            path: c.path.clone(),
            kind: if c.disabled {
                LocationKind::Disabled
            } else if c.is_symlink {
                LocationKind::Symlink {
                    target: c.real.clone(),
                }
            } else if c.real == origin {
                LocationKind::Origin
            } else {
                LocationKind::Copy
            },
        })
        .collect();

    let name = docs
        .name(&origin)
        .map(str::to_string)
        .or_else(|| {
            group
                .iter()
                .find(|c| c.real == origin)
                .map(|c| c.dir_name.clone())
        })
        .or_else(|| group.first().map(|c| c.dir_name.clone()))
        .unwrap_or_default();
    let (doc, parse_error) = docs.take(&origin);

    let codex_disabled =
        codex_disabled.contains(&origin) || group.iter().any(|c| codex_disabled.contains(&c.path));

    DiscoveredSkill {
        name,
        managed: origin.starts_with(store) || origin.starts_with(private),
        origin,
        locations,
        doc,
        parse_error,
        conflicts,
        codex_disabled,
    }
}

/// Convenience: scan the scopes under one home directory.
pub fn scan(roots: Roots) -> DiscoveryResult {
    Discovery::new(roots).run()
}

/// The agent an id belongs to, for callers holding a [`Location`].
pub fn agent_of(location: &Location) -> Option<&'static AgentDef> {
    Registry::get(location.agent_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixture::{Fixture, listing};

    #[test]
    fn finds_every_skill_directory_and_skips_everything_else() {
        let fx = Fixture::realistic();
        let result = fx.scan();

        let names: Vec<_> = result.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "broken-doc",
                "claude-only",
                "copied-around",
                "disabled-here",
                "hidden-one",
                "renamed-inside",
                "shared-one",
                "shared-two",
            ],
            "a directory without SKILL.md, a stray file and a broken link are not skills"
        );
    }

    #[test]
    fn the_frontmatter_name_wins_over_the_directory_name() {
        let fx = Fixture::realistic();
        let result = fx.scan();
        let skill = result.get("renamed-inside").unwrap();
        assert!(skill.origin.ends_with("wrong-dir-name"));
        assert!(result.get("wrong-dir-name").is_none());
    }

    #[test]
    fn origin_is_the_real_directory_and_symlinks_point_at_it() {
        let fx = Fixture::realistic();
        let result = fx.scan();

        let skill = result.get("shared-one").unwrap();
        assert_eq!(skill.origin, fx.shared().join("shared-one"));
        assert!(skill.managed);
        assert_eq!(
            skill.location("shared").unwrap().kind,
            LocationKind::Origin,
            "the shared directory holds the bytes"
        );
        assert_eq!(
            skill.location("claude-code").unwrap().kind,
            LocationKind::Symlink {
                target: fx.shared().join("shared-one")
            },
            "Claude Code reaches it through a relative symlink"
        );

        let claude_only = result.get("claude-only").unwrap();
        assert_eq!(
            claude_only.origin,
            fx.agent("claude-code").join("claude-only")
        );
        assert_eq!(
            claude_only.location("claude-code").unwrap().kind,
            LocationKind::Origin
        );
    }

    #[test]
    fn a_skill_in_the_shared_directory_is_managed() {
        let fx = Fixture::realistic();
        let result = fx.scan();
        let skill = result.get("shared-two").unwrap();

        assert_eq!(fx.store(), fx.shared(), "the store is the shared directory");
        assert_eq!(skill.origin, fx.store().join("shared-two"));
        assert!(
            skill.managed,
            "the origin is in a directory Skillbase owns, so it may move links to it"
        );
        assert_eq!(skill.location("shared").unwrap().kind, LocationKind::Origin);
        assert!(
            skill.location("store").is_none(),
            "there is no separate store scope any more"
        );
        assert!(skill.visible_to().contains(&"cursor"), "no link in between");
    }

    #[test]
    fn a_skill_in_the_private_directory_is_managed_and_not_shared() {
        let fx = Fixture::realistic();
        let result = fx.scan();
        let skill = result.get("hidden-one").unwrap();

        assert_eq!(skill.origin, fx.private().join("hidden-one"));
        assert!(skill.managed);
        assert_eq!(
            skill.location("private").unwrap().kind,
            LocationKind::Origin
        );
        assert!(!skill.is_present_in("shared"));
        assert_eq!(
            skill.visible_to(),
            ["claude-code"],
            "only the agent holding a link of its own reaches it"
        );
    }

    #[test]
    fn a_skill_owned_by_another_tool_is_unmanaged() {
        let fx = Fixture::realistic();
        let skill = fx.scan();
        let skill = skill.get("claude-only").unwrap();
        assert_eq!(skill.origin, fx.agent("claude-code").join("claude-only"));
        assert!(
            !skill.managed,
            "an origin outside the store and the private directory belongs to someone else"
        );
    }

    #[test]
    fn a_copy_fan_out_is_reported_as_a_conflict_not_hidden() {
        let fx = Fixture::realistic();
        let result = fx.scan();
        let skill = result.get("copied-around").unwrap();

        assert_eq!(
            skill.origin,
            fx.shared().join("copied-around"),
            "the shared directory wins over an agent-local copy"
        );
        assert!(skill.has_conflict());
        assert_eq!(
            skill.conflicts,
            [fx.agent("gemini-cli").join("copied-around")]
        );
        assert_eq!(
            skill.location("gemini-cli").unwrap().kind,
            LocationKind::Copy
        );
    }

    #[test]
    fn a_disabled_link_is_present_but_not_visible() {
        let fx = Fixture::realistic();
        let result = fx.scan();
        let skill = result.get("disabled-here").unwrap();

        let location = skill.location("claude-code").unwrap();
        assert_eq!(location.kind, LocationKind::Disabled);
        assert!(
            location
                .path
                .starts_with(fx.home().join(".claude/skills-disabled"))
        );
        assert!(!skill.is_present_in("claude-code"));
        assert!(!skill.visible_to().contains(&"claude-code"));
    }

    #[test]
    fn visible_to_infers_every_agent_that_reads_the_shared_directory() {
        let fx = Fixture::realistic();
        let result = fx.scan();
        let visible = result.get("shared-one").unwrap().visible_to();

        assert!(visible.contains(&"claude-code"), "linked explicitly");
        assert!(visible.contains(&"zed"), "Zed reads the shared directory");
        assert!(visible.contains(&"cursor"));
        assert!(!visible.contains(&"cline"), "Cline needs its own link");
        assert!(
            !visible.contains(&"shared"),
            "shared is a directory, not an agent"
        );

        let claude_only = result.get("claude-only").unwrap();
        assert_eq!(claude_only.visible_to(), ["claude-code"]);
    }

    #[test]
    fn a_codex_config_entry_overrides_presence() {
        let fx = Fixture::realistic();
        let result = fx.scan();

        let disabled = result.get("shared-one").unwrap();
        assert!(disabled.codex_disabled);
        assert!(
            disabled.is_present_in("shared"),
            "the skill is still on disk in the shared directory"
        );
        assert!(
            !disabled.visible_to().contains(&"codex"),
            "enabled = false wins over presence"
        );

        let enabled = result.get("shared-two").unwrap();
        assert!(!enabled.codex_disabled);
        assert!(enabled.visible_to().contains(&"codex"));
    }

    #[test]
    fn an_unparseable_skill_md_still_appears_with_its_error() {
        let fx = Fixture::realistic();
        let result = fx.scan();
        let skill = result.get("broken-doc").unwrap();

        assert!(skill.doc.is_none());
        assert!(matches!(
            skill.parse_error,
            Some(SkillError::MissingFrontmatter)
        ));
        assert_eq!(
            skill.name, "broken-doc",
            "it falls back to the directory name"
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("broken-doc") && w.contains("frontmatter"))
        );
    }

    #[test]
    fn broken_links_and_unreadable_entries_warn_instead_of_panicking() {
        let fx = Fixture::realistic();
        let result = fx.scan();

        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.starts_with("broken symlink") && w.contains("dangling")),
            "warnings were {:?}",
            result.warnings
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("skill-md-is-a-dir") && w.contains("is a directory")),
            "warnings were {:?}",
            result.warnings
        );
    }

    #[test]
    fn a_directory_that_cannot_be_read_warns_instead_of_failing() {
        use std::os::unix::fs::PermissionsExt;

        let fx = Fixture::realistic();
        let locked = fx.agent("cursor");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let result = fx.scan();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(!result.skills.is_empty(), "the rest of the scan carried on");
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.starts_with("cannot read") && w.contains(".cursor/skills")),
            "warnings were {:?}",
            result.warnings
        );
    }

    #[test]
    fn a_symlink_loop_warns_and_does_not_hang() {
        let fx = Fixture::realistic();
        fx.symlink_raw("loop-a", &fx.shared().join("loop-b"));
        fx.symlink_raw("loop-b", &fx.shared().join("loop-a"));
        let result = fx.scan();
        assert_eq!(
            result
                .warnings
                .iter()
                .filter(|w| w.contains("loop-"))
                .count(),
            2
        );
        assert!(result.get("loop-a").is_none());
    }

    #[test]
    fn discovery_writes_nothing() {
        let fx = Fixture::realistic();
        let before = listing(fx.home());
        let result = fx.scan();
        let after = listing(fx.home());
        assert!(!result.skills.is_empty());
        assert_eq!(before, after, "discovery must be strictly read-only");
    }

    #[test]
    fn scanning_an_empty_home_finds_nothing_and_warns_about_nothing() {
        let fx = Fixture::empty();
        let result = fx.scan();
        assert!(result.skills.is_empty());
        assert!(
            result.warnings.is_empty(),
            "an agent that is not installed is not a problem: {:?}",
            result.warnings
        );
    }

    #[test]
    fn counts_by_agent_follow_visibility() {
        let fx = Fixture::realistic();
        let result = fx.scan();
        let counts: BTreeMap<_, _> = result.counts_by_agent().into_iter().collect();

        // broken-doc, copied-around, disabled-here, renamed-inside, shared-one,
        // shared-two all sit in the shared directory.
        assert_eq!(counts["shared"], 6);
        assert_eq!(counts["zed"], 6, "Zed reads the shared directory");
        assert_eq!(counts["codex"], 5, "one of the six is disabled in config");
        assert_eq!(counts["cline"], 0, "Cline has no directory here");
        assert_eq!(
            counts["claude-code"], 3,
            "a link into the store, a skill of its own, and a link into the private directory"
        );
    }

    #[test]
    fn codex_disabled_paths_reads_only_the_disabled_entries() {
        let fx = Fixture::empty();
        let config = fx.home().join(".codex/config.toml");
        fx.write_file(
            ".codex/config.toml",
            "[[skills.config]]\npath = \"/a/one/SKILL.md\"\nenabled = false\n\n\
             [[skills.config]]\npath = \"/a/two/SKILL.md\"\nenabled = true\n\n\
             [[skills.config]]\npath = \"/a/three/SKILL.md\"\n",
        );
        let disabled = codex_disabled_paths(&config).unwrap();
        assert_eq!(disabled, [PathBuf::from("/a/one")]);
    }

    #[test]
    fn a_missing_codex_config_is_not_an_error() {
        let fx = Fixture::empty();
        assert!(
            codex_disabled_paths(&fx.home().join(".codex/config.toml"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_malformed_codex_config_warns_without_stopping_the_scan() {
        let fx = Fixture::realistic();
        fx.write_file(".codex/config.toml", "this is = = not toml\n");
        let result = fx.scan();
        assert!(!result.skills.is_empty());
        assert!(result.warnings.iter().any(|w| w.contains("config.toml")));
    }

    #[test]
    fn agent_of_maps_a_location_back_to_the_table() {
        let fx = Fixture::realistic();
        let result = fx.scan();
        let location = result
            .get("shared-one")
            .unwrap()
            .location("claude-code")
            .unwrap();
        assert_eq!(agent_of(location).unwrap().display_name, "Claude Code");
        let private = result
            .get("hidden-one")
            .unwrap()
            .location("private")
            .unwrap();
        assert!(
            agent_of(private).is_none(),
            "the private directory is not an agent"
        );
        let store = result
            .get("shared-two")
            .unwrap()
            .location("shared")
            .unwrap();
        assert_eq!(
            agent_of(store).unwrap().id,
            "shared",
            "the store carries the shared id, which the table does hold"
        );
    }

    /// Scans the real home directory and prints what it found.
    ///
    /// Ignored by default, because it depends on the machine it runs on. Run it
    /// with `cargo test -p skillbase-core -- --ignored --nocapture real_machine`
    /// to check the scan against a machine that has skills on it. It reads and
    /// prints; it writes nothing.
    #[test]
    #[ignore = "depends on the machine it runs on"]
    fn real_machine_scan() {
        let roots = Roots::discover().expect("a home directory");
        let result = Discovery::new(roots.clone()).run();

        println!("home: {}", roots.home().display());
        println!("\n{} skills\n", result.skills.len());
        for (id, count) in result.counts_by_agent() {
            let dir = Registry::get(id).map(|a| roots.agent_dir(a));
            let installed = dir.as_deref().map(Path::exists).unwrap_or(false);
            println!(
                "{id:>12}  {count:>3}{}",
                if installed { "" } else { "   (no directory)" }
            );
        }

        println!("\nfirst 10 skills:");
        for skill in result.skills.iter().take(10) {
            println!(
                "  {:<28} {:<8} {}",
                skill.name,
                if skill.managed { "managed" } else { "" },
                skill.visible_to().join(", ")
            );
        }

        let invisible: Vec<_> = result
            .skills
            .iter()
            .filter(|s| s.visible_to().is_empty())
            .collect();
        println!("\n{} skills no agent can reach:", invisible.len());
        for skill in &invisible {
            println!("  {} at {}", skill.name, skill.origin.display());
        }

        let conflicted: Vec<_> = result.skills.iter().filter(|s| s.has_conflict()).collect();
        println!("\n{} skills with more than one origin:", conflicted.len());
        for skill in &conflicted {
            println!("  {} origin {}", skill.name, skill.origin.display());
            for path in &skill.conflicts {
                println!("      also {}", path.display());
            }
        }

        let broken: Vec<_> = result
            .skills
            .iter()
            .filter(|s| s.parse_error.is_some())
            .collect();
        println!("\n{} skills whose SKILL.md did not parse", broken.len());
        for skill in &broken {
            println!("  {}: {:?}", skill.name, skill.parse_error);
        }

        let codex_off: Vec<_> = result
            .skills
            .iter()
            .filter(|s| s.codex_disabled)
            .map(|s| s.name.as_str())
            .collect();
        println!("\ndisabled in the Codex config: {codex_off:?}");

        println!("\n{} warnings:", result.warnings.len());
        for warning in &result.warnings {
            println!("  {warning}");
        }
    }
}
