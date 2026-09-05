//! The only code in this crate that changes the filesystem.
//!
//! Every operation returns an [`Outcome`] listing what it actually did, so the
//! interface can tell the user the filesystem consequence of a switch instead
//! of implying one.
//!
//! # Safety
//!
//! Three rules are enforced in code, not merely documented:
//!
//! 1. Every destructive operation runs its paths through
//!    [`Installer::ensure_in_scope`] first. A path outside the store and the
//!    agent directories is refused, including one that tries to climb out with
//!    `..`.
//! 2. Deleting never follows a symlink. A link is unlinked; only a directory
//!    proven real is removed recursively.
//! 3. A real directory is never removed by an operation that expected a link.
//!    Under [`LinkMode::Copy`] a copy is only removed when it carries the
//!    marker file this crate wrote, so a copy Skillbase did not make is left
//!    alone.
//!
//! The roots come in as a [`Roots`], so tests run against a temporary home and
//! can never touch the user's own.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

use crate::discovery::DiscoveredSkill;
use crate::doc::SkillDoc;
use crate::error::SkillError;
use crate::frontmatter::SkillFrontmatter;
use crate::registry::{AgentDef, DisableMode, LinkMode, Roots};
use crate::skill::{SKILL_FILE_NAME, Skill};
use crate::slug::is_kebab_case;

/// Written into every directory Skillbase copies, so that removing a copy can
/// be justified rather than guessed.
pub const COPY_MARKER: &str = ".skillbase-copy";

/// Anything that can go wrong while changing the filesystem.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum InstallError {
    /// The path is not inside the store or an agent directory. Refused before
    /// anything was touched.
    #[error("{path}: outside every directory Skillbase manages, refusing to touch it")]
    OutsideScope {
        /// The path that was refused.
        path: PathBuf,
    },

    /// A path that had to be a symlink is a real file or directory.
    #[error("{path}: a real {kind} is already here, not a link Skillbase made")]
    NotALink {
        /// The path in the way.
        path: PathBuf,
        /// What it turned out to be, `"directory"` or `"file"`.
        kind: &'static str,
    },

    /// A directory could not be shown to be a copy Skillbase made, so it was
    /// left alone.
    #[error("{path}: not a copy Skillbase made (no {COPY_MARKER}), refusing to remove it")]
    NotACopy {
        /// The directory that was left alone.
        path: PathBuf,
    },

    /// The destination is occupied.
    #[error("{path}: already exists")]
    AlreadyExists {
        /// The occupied path.
        path: PathBuf,
    },

    /// The origin is missing or is not a directory.
    #[error("{path}: no skill directory here")]
    MissingOrigin {
        /// The path that was expected to hold a skill.
        path: PathBuf,
    },

    /// This agent's directory is the shared directory, so it has no link of its
    /// own.
    #[error("{agent} reads the shared directory; link the skill to Shared instead")]
    CoveredByShared {
        /// The agent's display name.
        agent: &'static str,
    },

    /// A skill name that is not kebab-case.
    #[error("skill name `{name}` is not kebab-case")]
    InvalidName {
        /// The rejected name.
        name: String,
    },

    /// A filesystem call failed.
    #[error("{path}: {source}")]
    Io {
        /// The path being changed.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },

    /// Reading or writing a `SKILL.md` failed.
    #[error(transparent)]
    Skill(#[from] SkillError),
}

impl InstallError {
    fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// One thing an operation did, in terms a user can check against their disk.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Change {
    /// A symlink was created.
    CreatedSymlink {
        /// Where the link now is.
        path: PathBuf,
        /// What it points at, as written: relative to the link's directory.
        target: PathBuf,
    },
    /// A symlink was removed. The directory it pointed at was not touched.
    RemovedSymlink {
        /// Where the link was.
        path: PathBuf,
    },
    /// A directory was copied.
    Copied {
        /// The source.
        from: PathBuf,
        /// The copy.
        to: PathBuf,
    },
    /// A real directory was removed, with everything in it.
    RemovedDirectory {
        /// The directory that is gone.
        path: PathBuf,
    },
    /// A directory or link was moved.
    Moved {
        /// Where it was.
        from: PathBuf,
        /// Where it is now.
        to: PathBuf,
    },
    /// A directory was created.
    CreatedDirectory {
        /// The new directory.
        path: PathBuf,
    },
    /// A file was written.
    WroteFile {
        /// The file.
        path: PathBuf,
    },
    /// Nothing needed doing.
    NoChange {
        /// The path that was already as asked.
        path: PathBuf,
        /// Why nothing happened.
        reason: &'static str,
    },
}

impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreatedSymlink { path, target } => {
                write!(f, "linked {} -> {}", path.display(), target.display())
            }
            Self::RemovedSymlink { path } => write!(f, "removed the link {}", path.display()),
            Self::Copied { from, to } => {
                write!(f, "copied {} to {}", from.display(), to.display())
            }
            Self::RemovedDirectory { path } => write!(f, "deleted {}", path.display()),
            Self::Moved { from, to } => {
                write!(f, "moved {} to {}", from.display(), to.display())
            }
            Self::CreatedDirectory { path } => write!(f, "created {}", path.display()),
            Self::WroteFile { path } => write!(f, "wrote {}", path.display()),
            Self::NoChange { path, reason } => {
                write!(f, "{} was already {reason}", path.display())
            }
        }
    }
}

/// What one operation did, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// The changes, oldest first.
    pub changes: Vec<Change>,
}

impl Outcome {
    /// An outcome holding one change.
    pub fn one(change: Change) -> Self {
        Self {
            changes: vec![change],
        }
    }

    /// Adds a change.
    pub fn push(&mut self, change: Change) {
        self.changes.push(change);
    }

    /// True when the filesystem was not altered.
    pub fn is_noop(&self) -> bool {
        self.changes
            .iter()
            .all(|c| matches!(c, Change::NoChange { .. }))
    }

    /// One line per change, for a status area or a log.
    pub fn describe(&self) -> String {
        self.changes
            .iter()
            .map(Change::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// What a delete would remove, counted before anything is removed.
///
/// Delete is the one destructive action that confirms, and the confirmation
/// names real numbers rather than a guess.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeletePlan {
    /// The skill being removed.
    pub name: String,
    /// The real directory holding the bytes, when it is inside a managed scope.
    pub origin: Option<PathBuf>,
    /// Symlinks pointing at it, including any parked in a disabled directory.
    pub links: Vec<PathBuf>,
    /// Real directories that duplicate it.
    pub copies: Vec<PathBuf>,
    /// Paths that were skipped because they sit outside every managed scope.
    pub skipped: Vec<PathBuf>,
}

impl DeletePlan {
    /// How many symlinks would go.
    pub fn link_count(&self) -> usize {
        self.links.len()
    }

    /// How many duplicate directories would go.
    pub fn copy_count(&self) -> usize {
        self.copies.len()
    }

    /// How many paths would be removed in total.
    pub fn total(&self) -> usize {
        self.links.len() + self.copies.len() + usize::from(self.origin.is_some())
    }
}

/// Creates, moves and removes the links that make a skill visible.
#[derive(Debug, Clone)]
pub struct Installer {
    roots: Roots,
}

impl Installer {
    /// Operates on the directories under `roots`.
    pub fn new(roots: Roots) -> Self {
        Self { roots }
    }

    /// The roots this installer is allowed to touch.
    pub fn roots(&self) -> &Roots {
        &self.roots
    }

    /// Rejects any path that is not strictly inside the store or an agent
    /// directory.
    ///
    /// The path is normalized textually first, so `<store>/../../..` is refused
    /// even though nothing on disk was consulted. The normalized path comes
    /// back, and it is the one the caller must act on.
    ///
    /// Called at the top of every operation that writes.
    pub fn ensure_in_scope(&self, path: &Path) -> Result<PathBuf, InstallError> {
        let normalized = normalize_lexical(path).ok_or_else(|| InstallError::OutsideScope {
            path: path.to_path_buf(),
        })?;
        let inside = self.roots.scope_roots().into_iter().any(|root| {
            // Strictly inside: a scope root is not itself deletable.
            normalized.starts_with(&root) && normalized != root
        });
        if inside {
            Ok(normalized)
        } else {
            Err(InstallError::OutsideScope {
                path: path.to_path_buf(),
            })
        }
    }

    /// Makes `origin` reachable from `agent`'s global directory.
    ///
    /// Creates a *relative* symlink, so a home directory that moves does not
    /// break every link in it. Creates the agent directory if it is missing.
    ///
    /// Idempotent: a link already pointing at the origin is left alone. A link
    /// pointing somewhere else is repointed. Anything that is not a link is
    /// refused rather than clobbered.
    pub fn link(
        &self,
        name: &str,
        origin: &Path,
        agent: &AgentDef,
    ) -> Result<Outcome, InstallError> {
        if agent.covered_by_shared() {
            return Err(InstallError::CoveredByShared {
                agent: agent.display_name,
            });
        }
        let origin = self.ensure_in_scope(origin)?;
        require_dir(&origin)?;
        let dest = self.ensure_in_scope(&self.roots.agent_dir(agent).join(name))?;

        match agent.link_mode {
            LinkMode::Symlink => self.place_symlink(&dest, &origin),
            LinkMode::Copy => self.place_copy(&dest, &origin),
        }
    }

    /// Removes the link that makes a skill reachable from `agent`.
    ///
    /// Only ever removes a symlink, or — under [`LinkMode::Copy`] — a directory
    /// carrying the marker file this crate writes. A real directory is refused,
    /// because removing it would destroy the only copy of the skill.
    pub fn unlink(&self, name: &str, agent: &AgentDef) -> Result<Outcome, InstallError> {
        let dest = self.ensure_in_scope(&self.roots.agent_dir(agent).join(name))?;
        self.remove_link_at(&dest, agent.link_mode)
    }

    /// Moves a skill's origin into the store and leaves a symlink behind.
    ///
    /// After this the skill is *managed* and Skillbase may add or remove links
    /// to it freely. Nothing about adoption is automatic; the user asks for it
    /// per skill.
    ///
    /// If the symlink cannot be created the move is undone, so a failure leaves
    /// the skill exactly where it was.
    pub fn adopt(&self, name: &str, origin: &Path) -> Result<Outcome, InstallError> {
        let origin = self.ensure_in_scope(origin)?;
        require_dir(&origin)?;
        let store = self.roots.store_dir();
        if origin.starts_with(&store) {
            return Ok(Outcome::one(Change::NoChange {
                path: origin,
                reason: "in the store",
            }));
        }
        let dest = self.ensure_in_scope(&store.join(name))?;
        if fs::symlink_metadata(&dest).is_ok() {
            return Err(InstallError::AlreadyExists { path: dest });
        }
        create_dir_all(&store)?;

        fs::rename(&origin, &dest).map_err(|e| InstallError::io(&origin, e))?;
        let mut outcome = Outcome::one(Change::Moved {
            from: origin.clone(),
            to: dest.clone(),
        });

        match self.place_symlink(&origin, &dest) {
            Ok(linked) => {
                outcome.changes.extend(linked.changes);
                Ok(outcome)
            }
            Err(e) => {
                // Put it back rather than leave the skill in a third state.
                let _ = fs::rename(&dest, &origin);
                Err(e)
            }
        }
    }

    /// Moves a managed skill back out of the store to `dest`, the inverse of
    /// [`Installer::adopt`].
    ///
    /// `dest` is where the skill used to live: normally the symlink adoption
    /// left behind. The symlink is removed first and restored if the move
    /// fails.
    pub fn release(&self, name: &str, dest: &Path) -> Result<Outcome, InstallError> {
        let dest = self.ensure_in_scope(dest)?;
        let source = self.ensure_in_scope(&self.roots.store_dir().join(name))?;
        require_dir(&source)?;

        let mut outcome = Outcome::default();
        match fs::symlink_metadata(&dest) {
            Ok(meta) if meta.file_type().is_symlink() => {
                fs::remove_file(&dest).map_err(|e| InstallError::io(&dest, e))?;
                outcome.push(Change::RemovedSymlink { path: dest.clone() });
            }
            Ok(meta) => {
                return Err(InstallError::NotALink {
                    path: dest,
                    kind: if meta.is_dir() { "directory" } else { "file" },
                });
            }
            Err(_) => {
                if let Some(parent) = dest.parent() {
                    create_dir_all(parent)?;
                }
            }
        }

        if let Err(e) = fs::rename(&source, &dest) {
            // Restore the link so the skill stays reachable.
            let _ = self.place_symlink(&dest, &source);
            return Err(InstallError::io(&source, e));
        }
        outcome.push(Change::Moved {
            from: source,
            to: dest,
        });
        Ok(outcome)
    }

    /// Switches a skill off for one agent, the way that agent expresses it.
    ///
    /// Claude Code parks the link in `~/.claude/skills-disabled/`. Codex sets
    /// `enabled = false` in `~/.codex/config.toml` and leaves the directory
    /// alone. Every other agent has no such state, so this removes the link and
    /// the interface must say so.
    pub fn disable(
        &self,
        name: &str,
        origin: &Path,
        agent: &AgentDef,
    ) -> Result<Outcome, InstallError> {
        match agent.disable {
            DisableMode::RemoveLink => self.unlink(name, agent),
            DisableMode::MoveAside(_) => {
                let from = self.roots.agent_dir(agent).join(name);
                let Some(to) = self.roots.disabled_dir(agent).map(|dir| dir.join(name)) else {
                    return self.unlink(name, agent);
                };
                self.move_entry(&from, &to)
            }
            DisableMode::CodexConfig => self.set_codex_enabled(origin, false),
        }
    }

    /// Switches a skill back on for one agent, the inverse of
    /// [`Installer::disable`].
    pub fn enable(
        &self,
        name: &str,
        origin: &Path,
        agent: &AgentDef,
    ) -> Result<Outcome, InstallError> {
        match agent.disable {
            DisableMode::RemoveLink => self.link(name, origin, agent),
            DisableMode::MoveAside(_) => {
                let to = self.roots.agent_dir(agent).join(name);
                let Some(from) = self.roots.disabled_dir(agent).map(|dir| dir.join(name)) else {
                    return self.link(name, origin, agent);
                };
                if fs::symlink_metadata(&from).is_err() {
                    // Never disabled, or already put back: make sure the link
                    // exists and say nothing else happened.
                    return self.link(name, origin, agent);
                }
                self.move_entry(&from, &to)
            }
            DisableMode::CodexConfig => self.set_codex_enabled(origin, true),
        }
    }

    /// Counts what deleting this skill would remove, without removing anything.
    ///
    /// Each location is classified by what it is on disk right now, not by what
    /// discovery called it, and anything outside a managed scope is listed
    /// under [`DeletePlan::skipped`] instead of being removed.
    pub fn plan_delete(&self, skill: &DiscoveredSkill) -> DeletePlan {
        let mut plan = DeletePlan {
            name: skill.name.clone(),
            ..DeletePlan::default()
        };

        let origin = self.ensure_in_scope(&skill.origin).ok();
        for location in &skill.locations {
            let Ok(path) = self.ensure_in_scope(&location.path) else {
                plan.skipped.push(location.path.clone());
                continue;
            };
            if Some(&path) == origin.as_ref() {
                continue;
            }
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.file_type().is_symlink() => plan.links.push(path),
                Ok(meta) if meta.is_dir() => plan.copies.push(path),
                _ => {}
            }
        }
        if origin.is_none() && !skill.origin.as_os_str().is_empty() {
            plan.skipped.push(skill.origin.clone());
        }
        plan.origin = origin.filter(|p| p.is_dir());
        plan
    }

    /// Carries out a plan from [`Installer::plan_delete`].
    ///
    /// Links go first, then duplicate directories, then the origin, so a
    /// failure part-way never leaves a link pointing at nothing that the user
    /// cannot see. Symlinks are unlinked, never followed.
    pub fn delete(&self, plan: &DeletePlan) -> Result<Outcome, InstallError> {
        let mut outcome = Outcome::default();
        for path in &plan.links {
            let path = self.ensure_in_scope(path)?;
            if let Ok(meta) = fs::symlink_metadata(&path) {
                if !meta.file_type().is_symlink() {
                    return Err(InstallError::NotALink {
                        path,
                        kind: if meta.is_dir() { "directory" } else { "file" },
                    });
                }
                fs::remove_file(&path).map_err(|e| InstallError::io(&path, e))?;
                outcome.push(Change::RemovedSymlink { path });
            }
        }
        for path in plan.copies.iter().chain(plan.origin.iter()) {
            let path = self.ensure_in_scope(path)?;
            outcome.changes.extend(self.remove_real_dir(&path)?.changes);
        }
        Ok(outcome)
    }

    /// Creates a new skill in the store from a template.
    ///
    /// Returns the new directory and what was written.
    pub fn create(
        &self,
        name: &str,
        description: &str,
    ) -> Result<(PathBuf, Outcome), InstallError> {
        if !is_kebab_case(name) {
            return Err(InstallError::InvalidName {
                name: name.to_string(),
            });
        }
        let dir = self.ensure_in_scope(&self.roots.store_dir().join(name))?;
        if fs::symlink_metadata(&dir).is_ok() {
            return Err(InstallError::AlreadyExists { path: dir });
        }

        let mut frontmatter = SkillFrontmatter::new();
        frontmatter.set_name(name);
        frontmatter.set_description(description);
        let body = format!(
            "\n# {name}\n\nWrite the instructions the agent reads when this skill fires.\n"
        );
        Skill::new(&dir, SkillDoc::new(frontmatter, body)).save()?;

        Ok((
            dir.clone(),
            Outcome {
                changes: vec![
                    Change::CreatedDirectory { path: dir.clone() },
                    Change::WroteFile {
                        path: dir.join(SKILL_FILE_NAME),
                    },
                ],
            },
        ))
    }

    /// Creates or repoints a relative symlink at `dest`.
    fn place_symlink(&self, dest: &Path, origin: &Path) -> Result<Outcome, InstallError> {
        let parent = dest.parent().ok_or_else(|| InstallError::OutsideScope {
            path: dest.to_path_buf(),
        })?;
        let relative = relative_from(parent, origin);

        match fs::symlink_metadata(dest) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let current = fs::read_link(dest).unwrap_or_default();
                if current == relative || fs::canonicalize(dest).ok().as_deref() == Some(origin) {
                    return Ok(Outcome::one(Change::NoChange {
                        path: dest.to_path_buf(),
                        reason: "linked to this skill",
                    }));
                }
                fs::remove_file(dest).map_err(|e| InstallError::io(dest, e))?;
            }
            Ok(meta) => {
                return Err(InstallError::NotALink {
                    path: dest.to_path_buf(),
                    kind: if meta.is_dir() { "directory" } else { "file" },
                });
            }
            Err(_) => create_dir_all(parent)?,
        }

        symlink(&relative, dest).map_err(|e| InstallError::io(dest, e))?;
        Ok(Outcome::one(Change::CreatedSymlink {
            path: dest.to_path_buf(),
            target: relative,
        }))
    }

    /// Copies `origin` to `dest`, replacing an earlier copy of ours.
    fn place_copy(&self, dest: &Path, origin: &Path) -> Result<Outcome, InstallError> {
        let mut outcome = Outcome::default();
        if fs::symlink_metadata(dest).is_ok() {
            outcome
                .changes
                .extend(self.remove_link_at(dest, LinkMode::Copy)?.changes);
        }
        if let Some(parent) = dest.parent() {
            create_dir_all(parent)?;
        }
        copy_dir(origin, dest)?;
        let marker = dest.join(COPY_MARKER);
        fs::write(&marker, format!("{}\n", origin.display()))
            .map_err(|e| InstallError::io(&marker, e))?;
        outcome.push(Change::Copied {
            from: origin.to_path_buf(),
            to: dest.to_path_buf(),
        });
        Ok(outcome)
    }

    /// Removes a link, refusing anything this crate cannot show it owns.
    fn remove_link_at(&self, dest: &Path, mode: LinkMode) -> Result<Outcome, InstallError> {
        let meta = match fs::symlink_metadata(dest) {
            Ok(meta) => meta,
            Err(_) => {
                return Ok(Outcome::one(Change::NoChange {
                    path: dest.to_path_buf(),
                    reason: "not there",
                }));
            }
        };
        if meta.file_type().is_symlink() {
            fs::remove_file(dest).map_err(|e| InstallError::io(dest, e))?;
            return Ok(Outcome::one(Change::RemovedSymlink {
                path: dest.to_path_buf(),
            }));
        }
        if meta.is_dir() && mode == LinkMode::Copy {
            if !dest.join(COPY_MARKER).is_file() {
                return Err(InstallError::NotACopy {
                    path: dest.to_path_buf(),
                });
            }
            return self.remove_real_dir(dest);
        }
        Err(InstallError::NotALink {
            path: dest.to_path_buf(),
            kind: if meta.is_dir() { "directory" } else { "file" },
        })
    }

    /// Removes a directory that has been shown to be real, never a symlink.
    fn remove_real_dir(&self, path: &Path) -> Result<Outcome, InstallError> {
        let path = self.ensure_in_scope(path)?;
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => Err(InstallError::NotALink {
                path,
                kind: "symlink",
            }),
            Ok(meta) if meta.is_dir() => {
                fs::remove_dir_all(&path).map_err(|e| InstallError::io(&path, e))?;
                Ok(Outcome::one(Change::RemovedDirectory { path }))
            }
            Ok(_) => Err(InstallError::NotALink { path, kind: "file" }),
            Err(_) => Ok(Outcome::one(Change::NoChange {
                path,
                reason: "not there",
            })),
        }
    }

    /// Moves a link or directory between two managed directories.
    ///
    /// A symlink is rebuilt against its new parent rather than renamed, so a
    /// relative target stays correct however deep the two directories sit.
    fn move_entry(&self, from: &Path, to: &Path) -> Result<Outcome, InstallError> {
        let from = self.ensure_in_scope(from)?;
        let to = self.ensure_in_scope(to)?;
        let meta = match fs::symlink_metadata(&from) {
            Ok(meta) => meta,
            Err(_) => {
                return Ok(Outcome::one(Change::NoChange {
                    path: from,
                    reason: "not there",
                }));
            }
        };
        if fs::symlink_metadata(&to).is_ok() {
            return Err(InstallError::AlreadyExists { path: to });
        }
        if let Some(parent) = to.parent() {
            create_dir_all(parent)?;
        }

        if meta.file_type().is_symlink() {
            let target = fs::canonicalize(&from).map_err(|e| InstallError::io(&from, e))?;
            let mut outcome = self.place_symlink(&to, &target)?;
            fs::remove_file(&from).map_err(|e| InstallError::io(&from, e))?;
            outcome.push(Change::RemovedSymlink { path: from });
            return Ok(outcome);
        }

        fs::rename(&from, &to).map_err(|e| InstallError::io(&from, e))?;
        Ok(Outcome::one(Change::Moved { from, to }))
    }

    /// Writes `enabled` on the skill's `[[skills.config]]` entry in
    /// `~/.codex/config.toml`, preserving the rest of the file exactly.
    ///
    /// Codex keys the entry by the path of the `SKILL.md`. Turning a skill off
    /// appends an entry when there is none; turning one on that was never
    /// listed changes nothing, because listed-and-absent both mean enabled.
    fn set_codex_enabled(&self, origin: &Path, enabled: bool) -> Result<Outcome, InstallError> {
        let config = self.roots.codex_config();
        let skill_md = origin.join(SKILL_FILE_NAME);
        let key = skill_md.to_string_lossy().into_owned();

        let text = match fs::read_to_string(&config) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(InstallError::io(&config, e)),
        };
        let mut doc = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| InstallError::io(&config, std::io::Error::other(e.to_string())))?;

        let mut changed = false;
        let existing = doc
            .get_mut("skills")
            .and_then(|skills| skills.get_mut("config"))
            .and_then(|config| config.as_array_of_tables_mut())
            .and_then(|tables| {
                tables
                    .iter_mut()
                    .find(|t| t.get("path").and_then(|v| v.as_str()) == Some(key.as_str()))
            });

        if let Some(entry) = existing {
            if entry.get("enabled").and_then(|v| v.as_bool()) != Some(enabled) {
                entry["enabled"] = toml_edit::value(enabled);
                changed = true;
            }
        } else if !enabled {
            let tables = codex_config_array(&mut doc, &config)?;
            let mut entry = toml_edit::Table::new();
            entry["path"] = toml_edit::value(key);
            entry["enabled"] = toml_edit::value(false);
            tables.push(entry);
            changed = true;
        }

        if !changed {
            return Ok(Outcome::one(Change::NoChange {
                path: config,
                reason: if enabled {
                    "not disabled for Codex"
                } else {
                    "disabled for Codex"
                },
            }));
        }
        if let Some(parent) = config.parent() {
            create_dir_all(parent)?;
        }
        fs::write(&config, doc.to_string()).map_err(|e| InstallError::io(&config, e))?;
        Ok(Outcome::one(Change::WroteFile { path: config }))
    }
}

/// The `[[skills.config]]` array, created if the file has none.
fn codex_config_array<'a>(
    doc: &'a mut toml_edit::DocumentMut,
    config: &Path,
) -> Result<&'a mut toml_edit::ArrayOfTables, InstallError> {
    if doc.get("skills").is_none() {
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        doc.insert("skills", toml_edit::Item::Table(table));
    }
    let skills = doc["skills"].as_table_mut().ok_or_else(|| {
        InstallError::io(config, std::io::Error::other("`skills` is not a table"))
    })?;
    if skills.get("config").is_none() {
        skills.insert(
            "config",
            toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()),
        );
    }
    skills["config"].as_array_of_tables_mut().ok_or_else(|| {
        InstallError::io(
            config,
            std::io::Error::other("`skills.config` is not an array of tables"),
        )
    })
}

/// Fails unless `path` is a directory that exists.
fn require_dir(path: &Path) -> Result<(), InstallError> {
    if fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false) {
        Ok(())
    } else {
        Err(InstallError::MissingOrigin {
            path: path.to_path_buf(),
        })
    }
}

fn create_dir_all(path: &Path) -> Result<(), InstallError> {
    fs::create_dir_all(path).map_err(|e| InstallError::io(path, e))
}

/// Copies a directory tree, following nothing.
fn copy_dir(from: &Path, to: &Path) -> Result<(), InstallError> {
    create_dir_all(to)?;
    let entries = fs::read_dir(from).map_err(|e| InstallError::io(from, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| InstallError::io(from, e))?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let meta = fs::symlink_metadata(&source).map_err(|e| InstallError::io(&source, e))?;
        if meta.is_dir() {
            copy_dir(&source, &target)?;
        } else if meta.file_type().is_symlink() {
            let link = fs::read_link(&source).map_err(|e| InstallError::io(&source, e))?;
            symlink(&link, &target).map_err(|e| InstallError::io(&target, e))?;
        } else {
            fs::copy(&source, &target).map_err(|e| InstallError::io(&source, e))?;
        }
    }
    Ok(())
}

/// Resolves `.` and `..` textually, without touching the filesystem.
///
/// Returns `None` for a relative path: every path this crate writes to is
/// absolute, and treating a relative one as if it were rooted is how a guard
/// gets talked into leaving its own scope.
fn normalize_lexical(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    Some(out)
}

/// The shortest relative path from `from_dir` to `to`, both absolute.
///
/// `relative_from("~/.claude/skills", "~/.agents/skills/foo")` is
/// `../../.agents/skills/foo`, which is what the links on a real machine look
/// like and what keeps them working when a home directory moves.
fn relative_from(from_dir: &Path, to: &Path) -> PathBuf {
    let from: Vec<_> = from_dir.components().collect();
    let target: Vec<_> = to.components().collect();
    let common = from.iter().zip(&target).take_while(|(a, b)| a == b).count();

    let mut relative = PathBuf::new();
    for _ in common..from.len() {
        relative.push("..");
    }
    for component in &target[common..] {
        relative.push(component);
    }
    if relative.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        relative
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::LocationKind;
    use crate::registry::Registry;
    use crate::test_fixture::{Fixture, listing};

    fn claude() -> &'static AgentDef {
        Registry::get("claude-code").unwrap()
    }

    fn cursor() -> &'static AgentDef {
        Registry::get("cursor").unwrap()
    }

    // -- the guard -------------------------------------------------------

    #[test]
    fn the_guard_rejects_everything_outside_the_managed_scopes() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        let home = fx.home().to_path_buf();

        for path in [
            PathBuf::from("/"),
            home.clone(),
            home.join("Documents"),
            home.join("Documents/secret"),
            fx.store().join("../../.."),
            fx.store().join("../../../etc/passwd"),
            fx.shared().join("../../../tmp"),
            PathBuf::from("relative/path"),
            fx.store(),
            fx.shared(),
        ] {
            assert!(
                matches!(
                    installer.ensure_in_scope(&path),
                    Err(InstallError::OutsideScope { .. })
                ),
                "{} should have been refused",
                path.display()
            );
        }
    }

    #[test]
    fn the_guard_accepts_paths_inside_a_scope_and_normalizes_them() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        assert_eq!(
            installer.ensure_in_scope(&fx.store().join("a")).unwrap(),
            fx.store().join("a")
        );
        assert_eq!(
            installer
                .ensure_in_scope(&fx.shared().join("a/../b"))
                .unwrap(),
            fx.shared().join("b"),
            "`..` is resolved before the check, not after"
        );
        assert!(
            installer
                .ensure_in_scope(&fx.home().join(".claude/skills-disabled/a"))
                .is_ok()
        );
    }

    #[test]
    fn a_destructive_call_on_an_out_of_scope_path_touches_nothing() {
        let fx = Fixture::realistic();
        let before = listing(fx.home());
        let outside = fx.home().join("Documents");
        let plan = DeletePlan {
            name: "x".into(),
            origin: Some(outside.clone()),
            ..DeletePlan::default()
        };
        assert!(matches!(
            fx.installer().delete(&plan),
            Err(InstallError::OutsideScope { .. })
        ));
        assert_eq!(before, listing(fx.home()));
        assert!(outside.is_dir());
    }

    // -- relative links ---------------------------------------------------

    #[test]
    fn relative_from_climbs_out_and_back_down() {
        assert_eq!(
            relative_from(
                Path::new("/home/u/.claude/skills"),
                Path::new("/home/u/.agents/skills/foo")
            ),
            Path::new("../../.agents/skills/foo")
        );
        assert_eq!(
            relative_from(Path::new("/a/b"), Path::new("/a/b/c")),
            Path::new("c")
        );
        assert_eq!(
            relative_from(Path::new("/a/b"), Path::new("/a/b")),
            Path::new(".")
        );
        assert_eq!(
            relative_from(Path::new("/a"), Path::new("/x/y")),
            Path::new("../x/y")
        );
    }

    // -- link and unlink ---------------------------------------------------

    #[test]
    fn link_creates_a_relative_symlink_and_is_idempotent() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        let origin = fx.shared().join("shared-one");

        let outcome = installer.link("shared-one", &origin, cursor()).unwrap();
        let link = fx.agent("cursor").join("shared-one");
        assert_eq!(
            outcome.changes,
            [Change::CreatedSymlink {
                path: link.clone(),
                target: PathBuf::from("../../.agents/skills/shared-one"),
            }]
        );
        assert_eq!(
            fs::read_link(&link).unwrap(),
            Path::new("../../.agents/skills/shared-one")
        );
        assert!(link.join(SKILL_FILE_NAME).is_file(), "the link resolves");

        let again = installer.link("shared-one", &origin, cursor()).unwrap();
        assert!(again.is_noop());
    }

    #[test]
    fn link_repoints_a_link_that_points_somewhere_else() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        installer
            .link("shared-one", &fx.shared().join("shared-one"), cursor())
            .unwrap();
        let outcome = installer
            .link("shared-one", &fx.store().join("shared-two"), cursor())
            .unwrap();
        assert!(!outcome.is_noop());
        assert_eq!(
            fs::canonicalize(fx.agent("cursor").join("shared-one")).unwrap(),
            fx.store().join("shared-two")
        );
    }

    #[test]
    fn link_refuses_to_clobber_a_real_directory() {
        let fx = Fixture::realistic();
        let err = fx
            .installer()
            .link("claude-only", &fx.shared().join("shared-one"), claude())
            .unwrap_err();
        assert!(matches!(
            err,
            InstallError::NotALink {
                kind: "directory",
                ..
            }
        ));
        assert!(
            fx.agent("claude-code")
                .join("claude-only/SKILL.md")
                .is_file(),
            "the real skill is untouched"
        );
    }

    #[test]
    fn link_creates_a_missing_agent_directory() {
        let fx = Fixture::realistic();
        let warp = Registry::get("warp").unwrap();
        assert!(!fx.agent("warp").exists());
        fx.installer()
            .link("shared-one", &fx.shared().join("shared-one"), warp)
            .unwrap();
        assert!(
            fx.agent("warp")
                .join("shared-one")
                .join(SKILL_FILE_NAME)
                .is_file()
        );
    }

    #[test]
    fn link_refuses_an_agent_that_is_covered_by_shared() {
        let fx = Fixture::realistic();
        let err = fx
            .installer()
            .link(
                "shared-one",
                &fx.shared().join("shared-one"),
                Registry::get("zed").unwrap(),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            InstallError::CoveredByShared { agent: "Zed" }
        ));
    }

    #[test]
    fn unlink_removes_the_link_and_leaves_the_origin() {
        let fx = Fixture::realistic();
        let outcome = fx.installer().unlink("shared-one", claude()).unwrap();
        assert_eq!(
            outcome.changes,
            [Change::RemovedSymlink {
                path: fx.agent("claude-code").join("shared-one")
            }]
        );
        assert!(!fx.agent("claude-code").join("shared-one").exists());
        assert!(
            fx.shared()
                .join("shared-one")
                .join(SKILL_FILE_NAME)
                .is_file()
        );
    }

    #[test]
    fn unlink_refuses_a_real_directory() {
        let fx = Fixture::realistic();
        let err = fx.installer().unlink("claude-only", claude()).unwrap_err();
        assert!(matches!(
            err,
            InstallError::NotALink {
                kind: "directory",
                ..
            }
        ));
        assert!(fx.agent("claude-code").join("claude-only").is_dir());
    }

    #[test]
    fn unlink_something_that_is_not_there_is_a_no_op() {
        let fx = Fixture::realistic();
        assert!(
            fx.installer()
                .unlink("nothing", claude())
                .unwrap()
                .is_noop()
        );
    }

    #[test]
    fn link_and_unlink_round_trip_leaves_the_tree_as_it_was() {
        let fx = Fixture::realistic();
        let before = listing(fx.home());
        let installer = fx.installer();
        installer
            .link("shared-one", &fx.shared().join("shared-one"), cursor())
            .unwrap();
        assert_ne!(before, listing(fx.home()));
        installer.unlink("shared-one", cursor()).unwrap();
        assert_eq!(before, listing(fx.home()));
    }

    // -- copy mode ---------------------------------------------------------

    fn copying_agent() -> AgentDef {
        AgentDef {
            link_mode: LinkMode::Copy,
            ..*cursor()
        }
    }

    #[test]
    fn copy_mode_copies_the_tree_and_marks_it_as_ours() {
        let fx = Fixture::realistic();
        let agent = copying_agent();
        let outcome = fx
            .installer()
            .link("shared-one", &fx.shared().join("shared-one"), &agent)
            .unwrap();
        let copy = fx.agent("cursor").join("shared-one");
        assert!(matches!(outcome.changes[0], Change::Copied { .. }));
        assert!(copy.join(SKILL_FILE_NAME).is_file());
        assert!(!copy.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(copy.join(COPY_MARKER).is_file());

        fx.installer().unlink("shared-one", &agent).unwrap();
        assert!(!copy.exists());
    }

    #[test]
    fn copy_mode_refuses_to_remove_a_copy_it_cannot_prove_it_made() {
        let fx = Fixture::realistic();
        let agent = copying_agent();
        fx.skill(".cursor/skills/hand-made", "hand-made");
        let err = fx.installer().unlink("hand-made", &agent).unwrap_err();
        assert!(matches!(err, InstallError::NotACopy { .. }));
        assert!(fx.agent("cursor").join("hand-made/SKILL.md").is_file());
    }

    // -- adopt and release --------------------------------------------------

    #[test]
    fn adopt_moves_the_origin_into_the_store_and_leaves_a_link() {
        let fx = Fixture::realistic();
        let origin = fx.shared().join("shared-one");
        let outcome = fx.installer().adopt("shared-one", &origin).unwrap();

        assert_eq!(
            outcome.changes,
            [
                Change::Moved {
                    from: origin.clone(),
                    to: fx.store().join("shared-one"),
                },
                Change::CreatedSymlink {
                    path: origin.clone(),
                    target: PathBuf::from("../../.skillbase/store/shared-one"),
                },
            ]
        );
        assert!(fx.store().join("shared-one/SKILL.md").is_file());
        assert!(origin.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(
            fx.agent("claude-code")
                .join("shared-one/SKILL.md")
                .is_file(),
            "the link through the shared directory still resolves"
        );

        let skill = fx.scan();
        let adopted = skill.get("shared-one").unwrap();
        assert!(adopted.managed);
        assert_eq!(adopted.origin, fx.store().join("shared-one"));
    }

    #[test]
    fn adopt_and_release_round_trip() {
        let fx = Fixture::realistic();
        let before = listing(fx.home());
        let origin = fx.shared().join("shared-one");
        let installer = fx.installer();

        installer.adopt("shared-one", &origin).unwrap();
        let outcome = installer.release("shared-one", &origin).unwrap();
        assert_eq!(
            outcome.changes,
            [
                Change::RemovedSymlink {
                    path: origin.clone()
                },
                Change::Moved {
                    from: fx.store().join("shared-one"),
                    to: origin,
                },
            ]
        );
        assert_eq!(before, listing(fx.home()), "release undoes adopt exactly");
    }

    #[test]
    fn adopt_refuses_when_the_store_already_has_that_name() {
        let fx = Fixture::realistic();
        fx.skill(".skillbase/store/shared-one", "shared-one");
        let before = listing(fx.home());
        let err = fx
            .installer()
            .adopt("shared-one", &fx.shared().join("shared-one"))
            .unwrap_err();
        assert!(matches!(err, InstallError::AlreadyExists { .. }));
        assert_eq!(before, listing(fx.home()), "a refusal changes nothing");
    }

    #[test]
    fn adopting_something_already_in_the_store_does_nothing() {
        let fx = Fixture::realistic();
        let outcome = fx
            .installer()
            .adopt("shared-two", &fx.store().join("shared-two"))
            .unwrap();
        assert!(outcome.is_noop());
    }

    #[test]
    fn adopt_refuses_a_path_outside_the_scopes() {
        let fx = Fixture::realistic();
        let outside = fx.home().join("Documents/secret");
        assert!(matches!(
            fx.installer().adopt("secret", &outside),
            Err(InstallError::OutsideScope { .. })
        ));
        assert!(outside.is_dir());
    }

    // -- disable and enable -------------------------------------------------

    #[test]
    fn claude_code_disable_parks_the_link_in_skills_disabled() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        let origin = fx.shared().join("shared-one");

        installer.disable("shared-one", &origin, claude()).unwrap();
        let parked = fx.home().join(".claude/skills-disabled/shared-one");
        assert!(!fx.agent("claude-code").join("shared-one").exists());
        assert!(parked.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(
            parked.join(SKILL_FILE_NAME).is_file(),
            "the relative target was rebuilt against the new parent"
        );

        let result = fx.scan();
        let skill = result.get("shared-one").unwrap();
        assert_eq!(
            skill.location("claude-code").unwrap().kind,
            LocationKind::Disabled
        );
        assert!(!skill.visible_to().contains(&"claude-code"));

        installer.enable("shared-one", &origin, claude()).unwrap();
        assert!(!parked.exists());
        assert!(
            fx.scan()
                .get("shared-one")
                .unwrap()
                .visible_to()
                .contains(&"claude-code")
        );
    }

    #[test]
    fn disabling_an_agent_with_no_such_state_removes_the_link() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        let origin = fx.shared().join("shared-one");
        installer.link("shared-one", &origin, cursor()).unwrap();
        installer.disable("shared-one", &origin, cursor()).unwrap();
        assert!(!fx.agent("cursor").join("shared-one").exists());
        installer.enable("shared-one", &origin, cursor()).unwrap();
        assert!(fx.agent("cursor").join("shared-one").exists());
    }

    #[test]
    fn codex_disable_writes_the_config_and_preserves_the_rest_of_the_file() {
        let fx = Fixture::realistic();
        let codex = Registry::get("codex").unwrap();
        let config = fx.home().join(".codex/config.toml");
        let before = fs::read_to_string(&config).unwrap();

        let outcome = fx
            .installer()
            .disable("shared-two", &fx.store().join("shared-two"), codex)
            .unwrap();
        assert!(matches!(outcome.changes[0], Change::WroteFile { .. }));

        let after = fs::read_to_string(&config).unwrap();
        assert!(after.starts_with(&before), "nothing before was rewritten");
        assert!(after.contains("# a comment Skillbase must not eat"));
        assert!(
            fx.scan().get("shared-two").unwrap().codex_disabled,
            "discovery sees what install wrote"
        );

        fx.installer()
            .enable("shared-two", &fx.store().join("shared-two"), codex)
            .unwrap();
        let reenabled = fs::read_to_string(&config).unwrap();
        assert_eq!(
            reenabled.matches("enabled = false").count(),
            1,
            "only shared-two was re-enabled"
        );
        assert_eq!(reenabled.matches("enabled = true").count(), 1);
        assert!(reenabled.contains("# a comment Skillbase must not eat"));
        assert!(!fx.scan().get("shared-two").unwrap().codex_disabled);
    }

    #[test]
    fn codex_enable_flips_an_existing_entry_in_place() {
        let fx = Fixture::realistic();
        let codex = Registry::get("codex").unwrap();
        let origin = fx.shared().join("shared-one");
        assert!(fx.scan().get("shared-one").unwrap().codex_disabled);

        fx.installer().enable("shared-one", &origin, codex).unwrap();
        let text = fs::read_to_string(fx.home().join(".codex/config.toml")).unwrap();
        assert_eq!(
            text.matches("shared-one/SKILL.md").count(),
            1,
            "the entry was edited, not duplicated"
        );
        assert!(!fx.scan().get("shared-one").unwrap().codex_disabled);

        let again = fx.installer().enable("shared-one", &origin, codex).unwrap();
        assert!(again.is_noop());
    }

    // -- delete --------------------------------------------------------------

    #[test]
    fn plan_delete_counts_links_copies_and_the_origin() {
        let fx = Fixture::realistic();
        let result = fx.scan();
        let installer = fx.installer();

        let plan = installer.plan_delete(result.get("copied-around").unwrap());
        assert_eq!(plan.origin, Some(fx.shared().join("copied-around")));
        assert_eq!(plan.copy_count(), 1);
        assert_eq!(plan.link_count(), 0);
        assert_eq!(plan.total(), 2);

        let plan = installer.plan_delete(result.get("shared-two").unwrap());
        assert_eq!(plan.origin, Some(fx.store().join("shared-two")));
        assert_eq!(plan.link_count(), 1, "the shared directory links to it");
        assert_eq!(plan.total(), 2);

        let plan = installer.plan_delete(result.get("disabled-here").unwrap());
        assert_eq!(plan.link_count(), 1, "a parked link still counts");
    }

    #[test]
    fn delete_removes_every_link_and_then_the_origin() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        let plan = installer.plan_delete(fx.scan().get("shared-one").unwrap());
        assert_eq!(plan.link_count(), 1);

        let outcome = installer.delete(&plan).unwrap();
        assert_eq!(outcome.changes.len(), 2);
        assert!(!fx.shared().join("shared-one").exists());
        assert!(!fx.agent("claude-code").join("shared-one").exists());
        assert!(fx.scan().get("shared-one").is_none());
    }

    #[test]
    fn delete_never_follows_a_link_out_of_scope() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        // A link in a managed directory pointing at something precious.
        let precious = fx.home().join("Documents/secret");
        symlink(&precious, fx.agent("cursor").join("shared-one")).unwrap();

        let plan = DeletePlan {
            name: "shared-one".into(),
            origin: None,
            links: vec![fx.agent("cursor").join("shared-one")],
            ..DeletePlan::default()
        };
        installer.delete(&plan).unwrap();
        assert!(
            precious.join("keep-me.txt").is_file(),
            "unlinking must not touch what the link pointed at"
        );
    }

    #[test]
    fn delete_refuses_a_real_directory_listed_as_a_link() {
        let fx = Fixture::realistic();
        let plan = DeletePlan {
            name: "claude-only".into(),
            origin: None,
            links: vec![fx.agent("claude-code").join("claude-only")],
            ..DeletePlan::default()
        };
        assert!(matches!(
            fx.installer().delete(&plan),
            Err(InstallError::NotALink { .. })
        ));
        assert!(fx.agent("claude-code").join("claude-only").is_dir());
    }

    // -- create ---------------------------------------------------------------

    #[test]
    fn create_writes_a_valid_templated_skill_into_the_store() {
        let fx = Fixture::realistic();
        let (dir, outcome) = fx
            .installer()
            .create("new-thing", "Does a new thing. Use when asked.")
            .unwrap();

        assert_eq!(dir, fx.store().join("new-thing"));
        assert_eq!(outcome.changes.len(), 2);
        let skill = Skill::load(&dir).unwrap();
        assert!(skill.is_valid(), "{:?}", skill.validate());
        assert_eq!(skill.doc.frontmatter.name(), Some("new-thing"));
        assert_eq!(
            skill.doc.frontmatter.description(),
            Some("Does a new thing. Use when asked.")
        );

        let found = fx.scan();
        let created = found.get("new-thing").unwrap();
        assert!(created.managed);
        assert!(
            created.visible_to().is_empty(),
            "a new skill is linked to nothing"
        );
    }

    #[test]
    fn create_rejects_a_name_that_is_not_kebab_case() {
        let fx = Fixture::realistic();
        for name in ["Not Kebab", "under_score", "", "../escape"] {
            assert!(matches!(
                fx.installer().create(name, "d"),
                Err(InstallError::InvalidName { .. })
            ));
        }
        assert!(!fx.store().join("../escape").exists());
    }

    #[test]
    fn create_refuses_to_overwrite_an_existing_skill() {
        let fx = Fixture::realistic();
        assert!(matches!(
            fx.installer().create("shared-two", "d"),
            Err(InstallError::AlreadyExists { .. })
        ));
        assert!(
            fs::read_to_string(fx.store().join("shared-two/SKILL.md"))
                .unwrap()
                .contains("shared-two")
        );
    }

    #[test]
    fn create_then_link_makes_a_skill_visible_everywhere_shared_reaches() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        let (dir, _) = installer.create("new-thing", "A new thing.").unwrap();
        installer
            .link("new-thing", &dir, Registry::shared())
            .unwrap();

        let visible = fx.scan().get("new-thing").unwrap().visible_to();
        assert!(visible.contains(&"zed"));
        assert!(visible.contains(&"cursor"));
        assert!(!visible.contains(&"claude-code"));

        installer
            .link("new-thing", &dir, Registry::get("claude-code").unwrap())
            .unwrap();
        assert!(
            fx.scan()
                .get("new-thing")
                .unwrap()
                .visible_to()
                .contains(&"claude-code")
        );
    }
}
