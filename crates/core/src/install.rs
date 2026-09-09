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
//!    [`Installer::ensure_in_scope`] first. A path outside the store, the
//!    private directory and the agent directories is refused, including one
//!    that tries to climb out with `..`.
//! 2. Deleting never follows a symlink. A link is unlinked; a directory proven
//!    real is moved into `~/.skillbase/trash`, never destroyed, so a skill
//!    somebody wrote by hand can be got back out of Finder.
//! 3. A real directory is never removed by an operation that expected a link.
//!    Under [`LinkMode::Copy`] a copy is only removed when it carries the
//!    marker file this crate wrote, so a copy Skillbase did not make is left
//!    alone.
//!
//! The roots come in as a [`Roots`], so tests run against a temporary home and
//! can never touch the user's own.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead as _, BufReader};
use std::os::unix::fs::symlink;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::discovery::DiscoveredSkill;
use crate::doc::SkillDoc;
use crate::error::SkillError;
use crate::frontmatter::SkillFrontmatter;
use crate::registry::{AgentDef, DisableMode, LinkMode, Roots};
use crate::skill::{SKILL_FILE_NAME, Skill};
use crate::slug::{is_kebab_case, slugify};

/// Written into every directory Skillbase copies, so that removing a copy can
/// be justified rather than guessed.
pub const COPY_MARKER: &str = ".skillbase-copy";

/// Anything that can go wrong while changing the filesystem.
///
/// [`Display`] writes every path in full. [`InstallError::describe_under`]
/// writes the same message with the home directory as `~`, which is what the
/// interface shows; the two are one function so a message can never be written
/// twice and drift.
///
/// [`Display`]: std::fmt::Display
#[derive(Debug)]
#[non_exhaustive]
pub enum InstallError {
    /// The path is not inside the store or an agent directory. Refused before
    /// anything was touched.
    OutsideScope {
        /// The path that was refused.
        path: PathBuf,
    },

    /// A path that had to be a symlink is a real file or directory.
    NotALink {
        /// The path in the way.
        path: PathBuf,
        /// What it turned out to be, `"directory"` or `"file"`.
        kind: &'static str,
    },

    /// A directory could not be shown to be a copy Skillbase made, so it was
    /// left alone.
    NotACopy {
        /// The directory that was left alone.
        path: PathBuf,
    },

    /// The destination is occupied.
    AlreadyExists {
        /// The occupied path.
        path: PathBuf,
    },

    /// The origin is missing or is not a directory.
    MissingOrigin {
        /// The path that was expected to hold a skill.
        path: PathBuf,
    },

    /// A path picked for [`Installer::import`] does not hold a skill.
    ///
    /// The `hint` says which of the near misses it was — a path that is not
    /// there, a file rather than a directory, a directory with no `SKILL.md` —
    /// because "that is not a skill" on its own leaves the user to guess what
    /// would have been.
    NotASkill {
        /// The path that was picked.
        path: PathBuf,
        /// What to do instead, one sentence.
        hint: &'static str,
    },

    /// A path picked for [`Installer::import`] is already inside a directory
    /// Skillbase manages, so importing it would make a second copy of a skill
    /// the user already has.
    AlreadyManaged {
        /// The path that was picked.
        path: PathBuf,
    },

    /// This agent's directory is the shared directory, so it has no link of its
    /// own.
    CoveredByShared {
        /// The agent's display name.
        agent: &'static str,
    },

    /// A skill name that is not kebab-case.
    InvalidName {
        /// The rejected name.
        name: String,
    },

    /// A filesystem call failed.
    Io {
        /// The path being changed.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },

    /// An operation stopped after it had already changed the filesystem.
    ///
    /// Deleting a skill unlinks it from up to fourteen agents before it touches
    /// the origin, so "Permission denied" on the ninth link can mean eight
    /// links are already gone. Carrying what was done keeps the message honest:
    /// the interface lists the changes that stand and then says why the rest
    /// did not happen.
    ///
    /// Build it with [`InstallError::partial`], never by hand: an operation
    /// that failed before it changed anything must stay the plain error.
    Partial {
        /// What the operation had already done.
        done: Box<Outcome>,
        /// Why it stopped.
        source: Box<InstallError>,
    },

    /// An operation failed, and putting things back failed too.
    ///
    /// Both halves have to be said. The first names why the operation stopped;
    /// without the second the user is told about a failure while the skill sits
    /// in a state that neither the failure nor the operation describes.
    ///
    /// `cause` carries what still stands, as an [`InstallError::Partial`],
    /// whenever the operation knew: this is the one error where the disk is
    /// certainly not what the list shows, and [`InstallError::completed`] says
    /// so for every rollback whether the cause recorded the detail or not.
    Rollback {
        /// Why the operation stopped.
        cause: Box<InstallError>,
        /// Why undoing it failed.
        undo: Box<InstallError>,
        /// Where the skill's files are now, so the user can find them.
        at: PathBuf,
        /// True when the undo left symlinks pointing at a path that is gone,
        /// which the message has to say: the files being back where they were
        /// is not the whole state.
        dangling_links: bool,
    },

    /// Reading or writing a `SKILL.md` failed.
    Skill(SkillError),
}

impl InstallError {
    fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Attaches what an operation had already done to the error that stopped
    /// it.
    ///
    /// An operation that failed before it changed anything returns the plain
    /// error, so the ordinary refusal reads exactly as it always did.
    ///
    /// A `source` that is itself a [`InstallError::Partial`] is folded in
    /// rather than nested: a step of an operation can report its own
    /// half-finished work, and nesting would print two "Then it stopped" lines
    /// with a list of changes buried between them, and hide the inner changes
    /// from [`InstallError::completed`].
    pub fn partial(mut done: Outcome, source: InstallError) -> Self {
        if let Self::Partial {
            done: inner,
            source,
        } = source
        {
            done.changes.extend(inner.changes);
            return Self::partial(done, *source);
        }
        if done.is_noop() {
            return source;
        }
        Self::Partial {
            done: Box::new(done),
            source: Box::new(source),
        }
    }

    /// What the operation had already done before it failed, when it had done
    /// anything.
    ///
    /// `Some` means the filesystem is not what the caller last read, so the
    /// caller has to read it again. Every [`InstallError::Rollback`] answers
    /// `Some`, because a rollback is by definition a change that could not be
    /// put back: it reports the changes its `cause` recorded when there are
    /// any, and otherwise a single [`Change::NotUndone`] naming where the
    /// files were left.
    ///
    /// Borrowed except for that synthesized case, which is why it is a
    /// [`Cow`].
    pub fn completed(&self) -> Option<Cow<'_, Outcome>> {
        match self {
            Self::Partial { done, .. } => Some(Cow::Borrowed(done)),
            Self::Rollback { cause, at, .. } => Some(cause.completed().unwrap_or_else(|| {
                Cow::Owned(Outcome::one(Change::NotUndone {
                    path: at.to_path_buf(),
                }))
            })),
            _ => None,
        }
    }

    /// The message with `home` written as `~`.
    ///
    /// A refusal that spells out `/Users/someone/.claude/skills/x` is read less
    /// carefully than one that says `~/.claude/skills/x`, and the rest of the
    /// interface already abbreviates. The abbreviation is presentation, so it
    /// is a second rendering of the message rather than a change to the paths.
    pub fn describe_under(&self, home: &Path) -> String {
        let mut out = String::new();
        // Writing into a String cannot fail.
        let _ = self.write(&mut out, Some(home));
        out
    }

    fn write(&self, f: &mut impl std::fmt::Write, home: Option<&Path>) -> std::fmt::Result {
        let p = |path: &Path| match home {
            Some(home) => abbreviate(path, home),
            None => path.display().to_string(),
        };
        match self {
            Self::OutsideScope { path } => write!(
                f,
                "{} is outside every directory Skillbase manages. Move it under ~/.agents/skills, \
                 or delete it in Finder.",
                p(path)
            ),
            Self::NotALink { path, kind } => write!(
                f,
                "A real {kind} already sits at {}. Rename or remove it, then try again.",
                p(path)
            ),
            Self::NotACopy { path } => write!(
                f,
                "{} is not a copy Skillbase made, so it was left alone. Remove it in Finder if you \
                 no longer want it.",
                p(path)
            ),
            Self::AlreadyExists { path } => write!(
                f,
                "{} is already taken. Rename or remove what is there, then try again.",
                p(path)
            ),
            Self::MissingOrigin { path } => write!(
                f,
                "There is no skill directory at {}. Reload the list to see what is actually there.",
                p(path)
            ),
            Self::NotASkill { path, hint } => {
                write!(f, "{} is not a skill. {hint}", p(path))
            }
            Self::AlreadyManaged { path } => write!(
                f,
                "{} is a directory Skillbase already manages, or holds one, so importing it \
                 would make a second copy of a skill you already have. Open the skill in the \
                 list and use Adopt instead.",
                p(path)
            ),
            Self::CoveredByShared { agent } => write!(
                f,
                "{agent} reads the shared directory; link the skill to Shared instead"
            ),
            Self::InvalidName { name } => {
                write!(
                    f,
                    "`{name}` is not a usable skill name. Use lower-case words joined by hyphens, \
                     like `code-review`."
                )
            }
            Self::Io { path, source } => {
                if source.kind() == std::io::ErrorKind::PermissionDenied {
                    write!(
                        f,
                        "Skillbase is not allowed to write to {}. Check that path in Finder with \
                         File > Get Info, give yourself write access, then try again.",
                        p(path)
                    )
                } else {
                    write!(f, "{}: {source}", p(path))
                }
            }
            Self::Partial { done, source } => {
                let listed = match home {
                    Some(home) => done.describe_under(home),
                    None => done.describe(),
                };
                writeln!(f, "{listed}")?;
                write!(f, "Then it stopped: ")?;
                source.write(f, home)
            }
            Self::Rollback {
                cause,
                undo,
                at,
                dangling_links,
            } => {
                cause.write(f, home)?;
                writeln!(f)?;
                write!(f, "Putting things back failed too: ")?;
                undo.write(f, home)?;
                writeln!(f)?;
                if *dangling_links {
                    write!(
                        f,
                        "The skill's files are at {}, but some links to it may still point at the \
                         path it was moved to, which is no longer there. Check that path in \
                         Finder before trying again.",
                        p(at)
                    )
                } else {
                    write!(
                        f,
                        "The skill's files are at {}. Check that path in Finder before trying \
                         again.",
                        p(at)
                    )
                }
            }
            Self::Skill(source) => write!(f, "{source}"),
        }
    }
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.write(f, None)
    }
}

impl std::error::Error for InstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Partial { source, .. } => Some(source),
            Self::Rollback { cause, .. } => Some(cause),
            Self::Skill(source) => source.source(),
            _ => None,
        }
    }
}

impl From<SkillError> for InstallError {
    fn from(source: SkillError) -> Self {
        Self::Skill(source)
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
        /// What it pointed at, as written, so the link can be written again.
        /// `None` when it could not be read.
        target: Option<PathBuf>,
    },
    /// A directory was copied.
    Copied {
        /// The source.
        from: PathBuf,
        /// The copy.
        to: PathBuf,
    },
    /// A real directory was moved into the trash instead of being destroyed.
    MovedToTrash {
        /// Where the directory was.
        from: PathBuf,
        /// Where it is now, under `~/.skillbase/trash`.
        to: PathBuf,
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
    /// An operation changed the filesystem and could not put it back.
    ///
    /// No operation pushes this. [`InstallError::completed`] reports it for an
    /// [`InstallError::Rollback`] whose cause did not record what it had done,
    /// so a caller asking whether the disk changed is told yes rather than no.
    /// What changed is in the error's own message.
    NotUndone {
        /// Where the skill's files were left.
        path: PathBuf,
    },
}

impl Change {
    /// The change as one line, with `home` written as `~`.
    ///
    /// A notification that spells out `/Users/someone/.claude/skills/x` eight
    /// times is not read. The abbreviation is presentation, so it lives here
    /// rather than in the paths themselves.
    pub fn describe_under(&self, home: &Path) -> String {
        let mut out = String::new();
        // Writing into a String cannot fail.
        let _ = self.write(&mut out, Some(home));
        out
    }

    fn write(&self, f: &mut impl std::fmt::Write, home: Option<&Path>) -> std::fmt::Result {
        let p = |path: &Path| match home {
            Some(home) => abbreviate(path, home),
            None => path.display().to_string(),
        };
        match self {
            Self::CreatedSymlink { path, target } => {
                write!(f, "linked {} -> {}", p(path), target.display())
            }
            // The target is recorded so a restore can write the link again. It
            // is not worth a line in a notification, so it stays out of here.
            Self::RemovedSymlink { path, .. } => write!(f, "removed the link {}", p(path)),
            Self::Copied { from, to } => write!(f, "copied {} to {}", p(from), p(to)),
            Self::MovedToTrash { from, to } => {
                write!(f, "moved {} to the trash at {}", p(from), p(to))
            }
            Self::Moved { from, to } => write!(f, "moved {} to {}", p(from), p(to)),
            Self::CreatedDirectory { path } => write!(f, "created {}", p(path)),
            Self::WroteFile { path } => write!(f, "wrote {}", p(path)),
            Self::NoChange { path, reason } => write!(f, "{} was already {reason}", p(path)),
            Self::NotUndone { path } => {
                write!(f, "changed {} and could not put it back", p(path))
            }
        }
    }
}

impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.write(f, None)
    }
}

/// Writes `home` as `~`, leaving any path outside it alone.
fn abbreviate(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
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

    /// The same, with `home` written as `~` and a long list cut short.
    ///
    /// Consolidating eight copies produces sixteen changes. A notification
    /// listing all of them is a wall of text nobody reads, so past
    /// [`Self::MAX_DESCRIBED`] lines it names the first few and counts the
    /// rest.
    pub fn describe_under(&self, home: &Path) -> String {
        let mut lines: Vec<String> = self
            .changes
            .iter()
            .take(Self::MAX_DESCRIBED)
            .map(|change| change.describe_under(home))
            .collect();
        let remaining = self.changes.len().saturating_sub(Self::MAX_DESCRIBED);
        if remaining > 0 {
            lines.push(format!(
                "and {remaining} more change{}",
                if remaining == 1 { "" } else { "s" }
            ));
        }
        lines.join("\n")
    }

    /// How many changes a description names before it starts counting.
    const MAX_DESCRIBED: usize = 6;
}

/// What a restore put back, and what it could not.
///
/// A restore is offered from a notification, seconds after the delete, and the
/// user has already stopped watching. So it never fails outright: it puts back
/// everything it can and names the rest. A half-restore that says nothing is
/// the failure this type exists to prevent.
#[derive(Debug, Default, Clone)]
pub struct Restored {
    /// What went back on the disk.
    pub outcome: Outcome,
    /// What could not go back, and why.
    pub missed: Vec<Missed>,
}

impl Restored {
    /// True when nothing went back.
    pub fn is_noop(&self) -> bool {
        self.outcome.is_noop()
    }

    /// How many directories were moved back out of the trash.
    pub fn directories(&self) -> usize {
        self.outcome
            .changes
            .iter()
            .filter(|change| matches!(change, Change::Moved { .. }))
            .count()
    }

    /// How many links were written again.
    pub fn links(&self) -> usize {
        self.outcome
            .changes
            .iter()
            .filter(|change| matches!(change, Change::CreatedSymlink { .. }))
            .count()
    }

    /// One line per change and per miss, with `home` written as `~`.
    pub fn describe_under(&self, home: &Path) -> String {
        let mut lines = Vec::new();
        if !self.outcome.changes.is_empty() {
            lines.push(self.outcome.describe_under(home));
        }
        lines.extend(self.missed.iter().map(|miss| miss.describe_under(home)));
        lines.join("\n")
    }

    /// Records one thing that could not go back.
    fn miss(&mut self, path: PathBuf, reason: MissReason) {
        self.missed.push(Missed { path, reason });
    }
}

/// One thing a restore could not put back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missed {
    /// Where it was meant to go.
    pub path: PathBuf,
    /// Why it did not.
    pub reason: MissReason,
}

/// Why a restore left something out.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MissReason {
    /// Something else is at that path now.
    Occupied,
    /// What the delete took is no longer where it left it.
    Gone,
    /// Skillbase did not record what the link pointed at.
    TargetUnknown,
    /// The path is outside every directory Skillbase may write to.
    OutsideScope,
    /// The filesystem refused. Carries the message.
    Failed(String),
}

impl Missed {
    /// The miss as one sentence, with `home` written as `~`.
    pub fn describe_under(&self, home: &Path) -> String {
        let path = abbreviate(&self.path, home);
        match &self.reason {
            MissReason::Occupied => {
                format!("could not put back {path}: something else is there now")
            }
            MissReason::Gone => {
                format!("could not put back {path}: what the delete took is no longer in the trash")
            }
            MissReason::TargetUnknown => format!(
                "could not put back the link {path}: Skillbase did not record what it pointed at"
            ),
            MissReason::OutsideScope => format!(
                "could not put back {path}: it is outside the directories Skillbase may write to"
            ),
            MissReason::Failed(message) => format!("could not put back {path}: {message}"),
        }
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
    /// The real directory holding the bytes, when it is inside a managed scope
    /// and still on disk.
    pub origin: Option<PathBuf>,
    /// True when the origin is inside a managed scope but is no longer a
    /// directory: it went away between the scan and the plan.
    ///
    /// The other reason [`DeletePlan::origin`] is empty is an origin outside
    /// every managed scope, and that one is listed under
    /// [`DeletePlan::skipped`] instead. The two have to stay apart because
    /// they say opposite things about the disk: here the skill's bytes are
    /// already gone, there they are still where they were.
    pub origin_missing: bool,
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

/// How one duplicate directory's content compares to the origin's.
///
/// The comparison covers the whole skill directory, not just `SKILL.md`:
/// bundled scripts, references and assets count, because a duplicate that
/// carries a different `scripts/run.sh` is just as divergent as one with a
/// different description.
///
/// Every path is relative to the skill directory. A file that cannot be read
/// on either side is listed under [`ContentDiff::differing`] rather than
/// assumed equal, so an unreadable duplicate is never replaced by default.
///
/// The one thing left out of the comparison is [`COPY_MARKER`], the file
/// Skillbase writes into copies it makes itself. It is bookkeeping, not
/// content, and counting it would make every copy Skillbase made look
/// divergent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContentDiff {
    /// Paths both directories have, holding different bytes — or a file on one
    /// side and a directory or symlink on the other.
    pub differing: Vec<PathBuf>,
    /// Paths the duplicate has and the origin does not.
    pub extra: Vec<PathBuf>,
    /// Paths the origin has and the duplicate does not.
    pub missing: Vec<PathBuf>,
}

impl ContentDiff {
    /// True when the two directories hold exactly the same bytes.
    pub fn is_identical(&self) -> bool {
        self.differing.is_empty() && self.extra.is_empty() && self.missing.is_empty()
    }

    /// How many paths differ in any way.
    pub fn total(&self) -> usize {
        self.differing.len() + self.extra.len() + self.missing.len()
    }

    /// Every path involved, differing first, each relative to the skill
    /// directory.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.differing
            .iter()
            .chain(&self.extra)
            .chain(&self.missing)
            .map(PathBuf::as_path)
    }

    /// One line naming the counts, for a confirmation that has to be specific.
    pub fn summary(&self) -> String {
        if self.is_identical() {
            return "identical".to_string();
        }
        let mut parts = Vec::new();
        if !self.differing.is_empty() {
            let n = self.differing.len();
            parts.push(format!(
                "{n} file{} differ{}",
                plural(n),
                if n == 1 { "s" } else { "" }
            ));
        }
        if !self.extra.is_empty() {
            parts.push(format!(
                "{} extra file{}",
                self.extra.len(),
                plural(self.extra.len())
            ));
        }
        if !self.missing.is_empty() {
            parts.push(format!(
                "{} missing file{}",
                self.missing.len(),
                plural(self.missing.len())
            ));
        }
        parts.join(", ")
    }
}

/// One real directory that duplicates a skill's origin.
///
/// The fields are private on purpose. A duplicate whose content differs from
/// the origin is only ever replaced when [`ConsolidatePlan::force`] has been
/// called for its path, and keeping the flag out of reach of a struct literal
/// is what makes "forced by accident" impossible rather than merely unlikely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Duplicate {
    path: PathBuf,
    agent_id: &'static str,
    diff: ContentDiff,
    forced: bool,
}

impl Duplicate {
    /// The duplicate directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The scope it sits in: an agent id, or [`PRIVATE_ID`].
    ///
    /// [`PRIVATE_ID`]: crate::registry::PRIVATE_ID
    pub fn agent_id(&self) -> &'static str {
        self.agent_id
    }

    /// How its content compares to the origin's.
    pub fn diff(&self) -> &ContentDiff {
        &self.diff
    }

    /// True when it holds exactly what the origin holds.
    pub fn is_identical(&self) -> bool {
        self.diff.is_identical()
    }

    /// True when the caller has explicitly accepted losing this duplicate's
    /// differences.
    pub fn forced(&self) -> bool {
        self.forced
    }

    /// True when [`Installer::consolidate`] would replace it with a link.
    pub fn will_be_replaced(&self) -> bool {
        self.is_identical() || self.forced
    }
}

/// What consolidating a skill would replace, worked out before anything is
/// replaced.
///
/// Consolidation is destructive — it removes a real directory — so the plan is
/// computed first and every count in the confirmation comes from it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConsolidatePlan {
    name: String,
    origin: PathBuf,
    duplicates: Vec<Duplicate>,
    skipped: Vec<PathBuf>,
}

impl ConsolidatePlan {
    /// The skill being consolidated.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The directory that keeps the bytes. Every link will point here.
    pub fn origin(&self) -> &Path {
        &self.origin
    }

    /// Every duplicate directory, in registry order.
    pub fn duplicates(&self) -> &[Duplicate] {
        &self.duplicates
    }

    /// Paths left alone because they sit outside every directory Skillbase
    /// manages.
    pub fn skipped(&self) -> &[PathBuf] {
        &self.skipped
    }

    /// The duplicates that match the origin byte for byte.
    pub fn identical(&self) -> impl Iterator<Item = &Duplicate> {
        self.duplicates.iter().filter(|d| d.is_identical())
    }

    /// The duplicates that have diverged. These are skipped unless forced.
    pub fn differing(&self) -> impl Iterator<Item = &Duplicate> {
        self.duplicates.iter().filter(|d| !d.is_identical())
    }

    /// How many duplicates match the origin.
    pub fn identical_count(&self) -> usize {
        self.identical().count()
    }

    /// How many duplicates have diverged.
    pub fn differing_count(&self) -> usize {
        self.differing().count()
    }

    /// How many divergent duplicates the caller has explicitly forced.
    pub fn forced_count(&self) -> usize {
        self.differing().filter(|d| d.forced()).count()
    }

    /// How many duplicates would become links.
    pub fn replace_count(&self) -> usize {
        self.duplicates
            .iter()
            .filter(|d| d.will_be_replaced())
            .count()
    }

    /// How many duplicates would be left alone because they differ.
    pub fn skipped_count(&self) -> usize {
        self.differing().filter(|d| !d.forced()).count()
    }

    /// True when there is nothing to consolidate.
    pub fn is_empty(&self) -> bool {
        self.duplicates.is_empty()
    }

    /// Accept losing one divergent duplicate's differences.
    ///
    /// This is the only way to replace a duplicate whose content is not the
    /// origin's, and it names one path at a time: there is deliberately no
    /// "force everything" switch, because the whole point of the refusal is
    /// that each divergence is a decision the user has to look at.
    ///
    /// Returns true when a duplicate with that path was found.
    pub fn force(&mut self, path: &Path) -> bool {
        self.set_forced(path, true)
    }

    /// Take back a [`ConsolidatePlan::force`].
    pub fn unforce(&mut self, path: &Path) -> bool {
        self.set_forced(path, false)
    }

    /// True when this path has been forced.
    pub fn is_forced(&self, path: &Path) -> bool {
        self.duplicates.iter().any(|d| d.path == path && d.forced)
    }

    fn set_forced(&mut self, path: &Path, forced: bool) -> bool {
        match self.duplicates.iter_mut().find(|d| d.path == path) {
            Some(duplicate) => {
                duplicate.forced = forced;
                true
            }
            None => false,
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// How to import a directory from outside every managed scope.
///
/// The default takes the name from the skill's own frontmatter and refuses a
/// name that is taken, so an import never overwrites without being told to.
/// The two overrides are the two answers to that refusal: [`Self::named`] keeps
/// both, [`Self::replacing`] replaces. They are the same pair
/// [`InstallOptions`] offers a GitHub install, so the interface can put the
/// same question to the user either way.
///
/// [`InstallOptions`]: crate::InstallOptions
#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    /// Import under this name instead of the one in the frontmatter.
    pub name: Option<String>,
    /// Replace a skill of the same name already in the store. Off by default.
    pub replace: bool,
}

impl ImportOptions {
    /// The default: take the name from the skill, and refuse to overwrite.
    pub fn new() -> Self {
        Self::default()
    }

    /// Import under an explicit name, which is how "keep both" is asked for.
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Replace a skill of the same name, moving the old directory to the trash
    /// first rather than destroying it.
    pub fn replacing(mut self) -> Self {
        self.replace = true;
        self
    }
}

/// Creates, moves and removes the links that make a skill visible, and moves
/// the origins of the skills Skillbase owns.
///
/// The store is `~/.agents/skills`, the directory the agents read, so being
/// shared is a property of where the origin sits rather than of a link. Turning
/// Shared off moves the origin to `~/.skillbase/private`; turning it on moves it
/// back. See [`Installer::share`] and [`Installer::unshare`].
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

    /// Rejects any path that is not strictly inside the store, the private
    /// directory, the trash or an agent directory.
    ///
    /// The path is normalized textually first, so `<store>/../../..` is refused
    /// even though nothing on disk was consulted. The normalized path comes
    /// back, and it is the one the caller must act on.
    ///
    /// Called at the top of every operation that writes. That is why the trash
    /// counts here: a delete has to write into it. Whether Skillbase *manages*
    /// a path is the separate question `overlaps_managed` asks, and the trash
    /// is not in that list.
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
    ///
    /// The shared scope is the exception, because its directory is the store.
    /// Linking there is [`Installer::share`], which moves the origin instead of
    /// writing a link, and returns [`Change::NoChange`] when the origin is
    /// already in the store.
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
        if agent.is_shared() {
            return self.share(name, origin);
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
    ///
    /// The shared scope is the exception. Its directory is the store, so a real
    /// directory there is the skill's origin rather than a link, and unlinking
    /// it is [`Installer::unshare`]: the origin moves to the private directory
    /// and the links pointing at it follow.
    pub fn unlink(&self, name: &str, agent: &AgentDef) -> Result<Outcome, InstallError> {
        let dest = self.ensure_in_scope(&self.roots.agent_dir(agent).join(name))?;
        if agent.is_shared() && is_real_dir(&dest) {
            return self.unshare(name, &dest);
        }
        self.remove_link_at(&dest, agent.link_mode)
    }

    /// Turns the Shared switch on: puts the skill where every agent that reads
    /// `~/.agents/skills` finds it.
    ///
    /// That directory is the store, so this moves the origin rather than
    /// linking to it:
    ///
    /// - An origin already in the store is left alone, reported as
    ///   [`Change::NoChange`]. There is nothing to link, and a symlink from
    ///   `~/.agents/skills/<name>` to itself is never written.
    /// - An origin in the private directory moves back to
    ///   `~/.agents/skills/<name>`, and every symlink that pointed at it is
    ///   repointed, so no agent loses the skill.
    /// - An origin Skillbase does not own is linked into the shared directory
    ///   the way any other agent is linked. Moving another tool's directory
    ///   without being asked is what [`Installer::adopt`] is for.
    pub fn share(&self, name: &str, origin: &Path) -> Result<Outcome, InstallError> {
        let origin = self.ensure_in_scope(origin)?;
        require_dir(&origin)?;
        let store = self.roots.store_dir();
        if origin.starts_with(&store) {
            return Ok(Outcome::one(Change::NoChange {
                path: origin,
                reason: "in the shared directory",
            }));
        }
        if origin.starts_with(self.roots.private_dir()) {
            return self.move_origin(&origin, &store.join(name));
        }
        let dest = self.ensure_in_scope(&store.join(name))?;
        self.place_symlink(&dest, &origin)
    }

    /// Turns the Shared switch off: takes the skill out of the directory the
    /// agents read, without taking it away from the agents linked to it.
    ///
    /// - An origin in the store moves to `~/.skillbase/private/<name>`, and
    ///   every symlink that pointed at it is repointed there. An agent holding
    ///   its own link — Claude Code, say — keeps the skill; the agents that
    ///   were reading `~/.agents/skills` no longer see it.
    /// - An origin already in the private directory is left alone.
    /// - For an origin Skillbase does not own, the shared directory holds a
    ///   plain link, and removing that link is all this does.
    pub fn unshare(&self, name: &str, origin: &Path) -> Result<Outcome, InstallError> {
        let origin = self.ensure_in_scope(origin)?;
        let private = self.roots.private_dir();
        if origin.starts_with(&private) {
            return Ok(Outcome::one(Change::NoChange {
                path: origin,
                reason: "out of the shared directory",
            }));
        }
        if origin.starts_with(self.roots.store_dir()) {
            require_dir(&origin)?;
            return self.move_origin(&origin, &private.join(name));
        }
        let dest = self.ensure_in_scope(&self.roots.shared_dir().join(name))?;
        self.remove_link_at(&dest, LinkMode::Symlink)
    }

    /// Moves a skill's origin into the store — `~/.agents/skills/<name>` — and
    /// leaves a symlink behind at the path it came from.
    ///
    /// After this the skill is *managed* and Skillbase may add or remove links
    /// to it freely. It is also shared, because the store is the directory the
    /// agents read. Nothing about adoption is automatic; the user asks for it
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
        // An origin in the private directory is already managed; sharing it is
        // a move, not an adoption.
        if origin.starts_with(self.roots.private_dir()) {
            return self.share(name, &origin);
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
            Err(cause) => {
                // Put it back rather than leave the skill in a third state —
                // and if it will not go back, say where it ended up, because
                // the skill is now neither where it was nor where it was going.
                match fs::rename(&dest, &origin) {
                    Ok(()) => Err(cause),
                    // The move stands, so it is carried on the cause: the
                    // caller has to know the origin is at `dest` now.
                    Err(e) => Err(InstallError::Rollback {
                        cause: Box::new(InstallError::partial(outcome, cause)),
                        undo: Box::new(InstallError::io(&dest, e)),
                        at: dest,
                        dangling_links: false,
                    }),
                }
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
                // Read before removing: afterwards there is nothing left to
                // read, and a restore needs the target to write the link again.
                let target = fs::read_link(&dest).ok();
                fs::remove_file(&dest).map_err(|e| InstallError::io(&dest, e))?;
                outcome.push(Change::RemovedSymlink {
                    path: dest.clone(),
                    target,
                });
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
            let cause = InstallError::io(&source, e);
            // Restore the link so the skill stays reachable. If that fails the
            // link this call removed is gone for good, so the message has to
            // name the store as where the skill still is.
            if outcome.is_noop() {
                return Err(cause);
            }
            return Err(match self.place_symlink(&dest, &source) {
                Ok(_) => cause,
                // The removed link stays removed, so it is carried on the
                // cause rather than dropped.
                Err(undo) => InstallError::Rollback {
                    cause: Box::new(InstallError::partial(outcome, cause)),
                    undo: Box::new(undo),
                    at: source,
                    dangling_links: false,
                },
            });
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
    /// under [`DeletePlan::skipped`] instead of being removed. The origin is
    /// read the same way: one that is no longer a directory leaves
    /// [`DeletePlan::origin`] empty and sets [`DeletePlan::origin_missing`].
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
        // Recorded before `origin` is emptied, because afterwards there is no
        // way to tell an origin that is gone from one outside every scope, and
        // a caller that acts on the skill being gone needs to know which it is.
        plan.origin_missing = origin.as_ref().is_some_and(|path| !path.is_dir());
        plan.origin = origin.filter(|p| p.is_dir());
        plan
    }

    /// Carries out a plan from [`Installer::plan_delete`].
    ///
    /// Links go first, then duplicate directories, then the origin, so a
    /// failure part-way never leaves a link pointing at nothing that the user
    /// cannot see. Symlinks are unlinked, never followed; a real directory is
    /// moved into the trash rather than destroyed.
    ///
    /// A delete that stops part-way fails with [`InstallError::Partial`],
    /// carrying the links and directories it had already dealt with. Without
    /// that the user is told "Permission denied" and left to guess whether six
    /// agents have lost the skill.
    pub fn delete(&self, plan: &DeletePlan) -> Result<Outcome, InstallError> {
        let mut outcome = Outcome::default();
        match self.delete_into(plan, &mut outcome) {
            Ok(()) => Ok(outcome),
            Err(e) => Err(InstallError::partial(outcome, e)),
        }
    }

    /// The body of [`Installer::delete`], writing what it did into `outcome` as
    /// it goes so that a failure can still report it.
    fn delete_into(&self, plan: &DeletePlan, outcome: &mut Outcome) -> Result<(), InstallError> {
        for path in &plan.links {
            let path = self.ensure_in_scope(path)?;
            if let Ok(meta) = fs::symlink_metadata(&path) {
                if !meta.file_type().is_symlink() {
                    return Err(InstallError::NotALink {
                        path,
                        kind: if meta.is_dir() { "directory" } else { "file" },
                    });
                }
                // Read before removing, so a restore can put the link back
                // pointing where it pointed.
                let target = fs::read_link(&path).ok();
                fs::remove_file(&path).map_err(|e| InstallError::io(&path, e))?;
                outcome.push(Change::RemovedSymlink { path, target });
            }
        }
        for path in plan.copies.iter().chain(plan.origin.iter()) {
            let path = self.ensure_in_scope(path)?;
            outcome.changes.extend(self.remove_real_dir(&path)?.changes);
        }
        Ok(())
    }

    /// Put back what a delete took away.
    ///
    /// Pass the changes an [`Installer::delete`] reported.
    ///
    /// The changes are walked in reverse, because a delete removes the links
    /// first and moves the directories after. Going back the other way round
    /// means every directory is out of the trash before a link is written at
    /// it, so no link ever points at nothing.
    ///
    /// Never returns an error. A restore is offered from a notification the
    /// user is about to dismiss, so it does as much as it can and reports the
    /// rest in [`Restored::missed`] rather than stopping at the first refusal
    /// and leaving the skill in a third state.
    ///
    /// Only [`Change::MovedToTrash`] and [`Change::RemovedSymlink`] mean
    /// anything here: a restore reverses a delete, and every other change came
    /// from some other operation. Both ends of every write go through
    /// [`Installer::ensure_in_scope`], because the changes are a plain value a
    /// caller can build by hand and a restore must not write anywhere a delete
    /// could not have written.
    pub fn restore(&self, changes: &[Change]) -> Restored {
        let mut restored = Restored::default();
        for change in changes.iter().rev() {
            match change {
                Change::MovedToTrash { from, to } => {
                    self.restore_directory(from, to, &mut restored);
                }
                Change::RemovedSymlink { path, target } => {
                    self.restore_link(path, target.as_deref(), &mut restored);
                }
                _ => {}
            }
        }
        restored
    }

    /// Moves one directory back out of the trash to where it was.
    ///
    /// `from` is where it lived, `to` where the delete put it: the same two
    /// paths [`Change::MovedToTrash`] carries, used the other way round.
    fn restore_directory(&self, from: &Path, to: &Path, restored: &mut Restored) {
        let (Ok(from), Ok(to)) = (self.ensure_in_scope(from), self.ensure_in_scope(to)) else {
            restored.miss(from.to_path_buf(), MissReason::OutsideScope);
            return;
        };
        // Never clobber. Something at the old path is a skill the user has put
        // there since, and a restore that silently replaced it would be the
        // same loss the restore exists to undo.
        if fs::symlink_metadata(&from).is_ok() {
            restored.miss(from, MissReason::Occupied);
            return;
        }
        // The user emptied the trash, or moved the directory out of it by hand.
        if fs::symlink_metadata(&to).is_err() {
            restored.miss(from, MissReason::Gone);
            return;
        }
        if let Some(parent) = from.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            restored.miss(from, MissReason::Failed(e.to_string()));
            return;
        }

        if let Err(rename_error) = fs::rename(&to, &from) {
            if !crosses_filesystems(&rename_error) {
                restored.miss(from, MissReason::Failed(rename_error.to_string()));
                return;
            }
            // The trash and the directory the skill came from can sit on
            // different volumes, which is the one case `move_to_trash` copies
            // its way past on the way in.
            if let Err(e) = copy_dir(&to, &from) {
                restored.miss(from, MissReason::Failed(e.to_string()));
                return;
            }
            // The skill is back either way. A copy left behind in the trash is
            // not a miss: nothing reads the trash, and saying the restore
            // failed when the files are where the user asked for them would be
            // worse than saying nothing.
            let _ = fs::remove_dir_all(&to);
        }
        restored.outcome.push(Change::Moved { from: to, to: from });
    }

    /// Writes one removed link again, exactly as it was written.
    ///
    /// The recorded target is used verbatim rather than computed afresh: a
    /// link the user wrote by hand may be absolute, or relative through a path
    /// this crate would not have chosen, and a restore that changed it would
    /// not be a restore.
    fn restore_link(&self, path: &Path, target: Option<&Path>, restored: &mut Restored) {
        let Ok(path) = self.ensure_in_scope(path) else {
            restored.miss(path.to_path_buf(), MissReason::OutsideScope);
            return;
        };
        let Some(target) = target else {
            restored.miss(path, MissReason::TargetUnknown);
            return;
        };
        if fs::symlink_metadata(&path).is_ok() {
            restored.miss(path, MissReason::Occupied);
            return;
        }
        if let Some(parent) = path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            restored.miss(path, MissReason::Failed(e.to_string()));
            return;
        }
        if let Err(e) = symlink(target, &path) {
            restored.miss(path, MissReason::Failed(e.to_string()));
            return;
        }
        restored.outcome.push(Change::CreatedSymlink {
            path,
            target: target.to_path_buf(),
        });
    }

    /// Works out what consolidating this skill would replace, without
    /// replacing anything.
    ///
    /// A *duplicate* is a real directory, other than the origin, holding the
    /// same skill. The machines this exists for have up to eight of them per
    /// skill — one per agent that got a copy instead of a link — and they
    /// drift, because editing one changes nothing for the other seven.
    ///
    /// Each duplicate is compared against the origin file by file, so the
    /// caller can tell an exact copy (safe to replace with a link) from one
    /// that has been edited since (not safe, and refused by
    /// [`Installer::consolidate`] unless [`ConsolidatePlan::force`] says
    /// otherwise).
    ///
    /// Reads only. Anything outside the managed scopes lands in
    /// [`ConsolidatePlan::skipped`] and is never looked at again.
    pub fn plan_consolidate(&self, skill: &DiscoveredSkill) -> ConsolidatePlan {
        let mut plan = ConsolidatePlan {
            name: skill.name.clone(),
            ..ConsolidatePlan::default()
        };
        let Ok(origin) = self.ensure_in_scope(&skill.origin) else {
            // Without an origin inside a scope there is nothing to point links
            // at, so every location is left alone.
            plan.skipped = skill.locations.iter().map(|l| l.path.clone()).collect();
            return plan;
        };
        plan.origin = origin.clone();

        // Locations first, because they carry the agent each path belongs to.
        // `conflicts` is the same set of directories without that label, so it
        // only contributes anything a location did not already cover.
        let paths = skill
            .locations
            .iter()
            .map(|location| (location.path.clone(), location.agent_id))
            .chain(skill.conflicts.iter().map(|path| (path.clone(), "")));

        for (path, agent_id) in paths {
            let Ok(path) = self.ensure_in_scope(&path) else {
                if !plan.skipped.contains(&path) {
                    plan.skipped.push(path);
                }
                continue;
            };
            if path == origin || plan.duplicates.iter().any(|d| d.path == path) {
                continue;
            }
            // Only a real directory is a duplicate. A symlink already points at
            // the origin, or at something else this operation has no business
            // rewriting.
            match fs::symlink_metadata(&path) {
                Ok(meta) if !meta.file_type().is_symlink() && meta.is_dir() => {}
                _ => continue,
            }
            plan.duplicates.push(Duplicate {
                diff: compare_dirs(&origin, &path),
                path,
                agent_id,
                forced: false,
            });
        }
        plan
    }

    /// Carries out a plan from [`Installer::plan_consolidate`]: every duplicate
    /// directory becomes a relative symlink to the origin.
    ///
    /// # What it refuses
    ///
    /// - A duplicate whose content differs from the origin is **skipped**, not
    ///   removed, unless [`ConsolidatePlan::force`] named its path. Replacing
    ///   it would throw away edits the user made and cannot get back.
    /// - The comparison is run again here rather than trusted from the plan, so
    ///   a duplicate edited between the confirmation and the click is still
    ///   caught.
    /// - Every path goes through [`Installer::ensure_in_scope`] first.
    /// - A symlink is never followed and never removed as if it were a copy: a
    ///   duplicate that is already a link is a no-op.
    /// - The origin is never a candidate for removal, even if a caller lists
    ///   it.
    ///
    /// Each replacement removes the duplicate and then links it, in that order,
    /// so a failure leaves at worst a missing copy whose bytes are still at the
    /// origin — never a link pointing at nothing.
    /// A consolidation that stops part-way fails with
    /// [`InstallError::Partial`], carrying the directories it had already moved
    /// to the trash and the links it had already written. It has to: by the
    /// time anything can fail it has usually taken a real directory away, and a
    /// message that says only why it stopped hides that.
    pub fn consolidate(&self, plan: &ConsolidatePlan) -> Result<Outcome, InstallError> {
        let mut outcome = Outcome::default();
        match self.consolidate_into(plan, &mut outcome) {
            Ok(()) => Ok(outcome),
            Err(e) => Err(InstallError::partial(outcome, e)),
        }
    }

    /// The body of [`Installer::consolidate`], writing what it did into
    /// `outcome` as it goes so that a failure can still report it.
    fn consolidate_into(
        &self,
        plan: &ConsolidatePlan,
        outcome: &mut Outcome,
    ) -> Result<(), InstallError> {
        let origin = self.ensure_in_scope(plan.origin())?;
        require_dir(&origin)?;

        for duplicate in plan.duplicates() {
            let path = self.ensure_in_scope(duplicate.path())?;
            if path == origin || origin.starts_with(&path) {
                outcome.push(Change::NoChange {
                    path,
                    reason: "the origin itself",
                });
                continue;
            }

            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    outcome.push(Change::NoChange {
                        path,
                        reason: "a link, not a copy",
                    });
                    continue;
                }
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => {
                    return Err(InstallError::NotALink { path, kind: "file" });
                }
                Err(_) => {
                    outcome.push(Change::NoChange {
                        path,
                        reason: "not there",
                    });
                    continue;
                }
            }

            if !duplicate.forced() && !compare_dirs(&origin, &path).is_identical() {
                outcome.push(Change::NoChange {
                    path,
                    reason: "different from the origin, so it was left alone",
                });
                continue;
            }

            outcome.changes.extend(self.remove_real_dir(&path)?.changes);
            outcome
                .changes
                .extend(self.place_symlink(&path, &origin)?.changes);
        }
        Ok(())
    }

    /// Creates a new skill in the store from a template.
    ///
    /// The store is `~/.agents/skills`, so a new skill is shared from the
    /// moment it is written: every agent that reads that directory sees it, and
    /// only the agents with a directory of their own still need a link.
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

    /// Copies a skill directory from anywhere on disk into the store, so
    /// Skillbase manages it from then on.
    ///
    /// This is the route in for a skill that is already on the machine and
    /// nowhere Skillbase looks: a directory in `~/Downloads`, or one inside a
    /// repository the user has cloned. The alternative is copying it into
    /// `~/.agents/skills` in Finder, which is a thing the interface can hardly
    /// ask for.
    ///
    /// It **copies**. The user's directory is left exactly where it is, so a
    /// skill imported out of a checkout is still in the checkout afterwards.
    /// That is the difference from [`Installer::adopt`], which moves the origin
    /// and leaves a symlink, and the returned [`Change::Copied`] is what says
    /// which of the two happened.
    ///
    /// The name is the frontmatter `name`, the same name discovery would show,
    /// slugified when it is not already kebab-case, and taken from the
    /// directory name when the frontmatter has none. [`ImportOptions::named`]
    /// overrides it. Either way the name is validated the way
    /// [`Installer::create`] validates one, because it becomes a directory
    /// name.
    ///
    /// Returns the new directory and what was copied.
    ///
    /// # Refusals
    ///
    /// Everything is checked before anything is written:
    ///
    /// - A path that is not there, is not a directory, or holds no readable
    ///   `SKILL.md` is [`InstallError::NotASkill`], carrying a sentence that
    ///   says what to pick instead. Picking a repository rather than the skill
    ///   inside it is the common mistake, and it is the one that sentence is
    ///   written for.
    /// - A path inside a directory Skillbase already manages is
    ///   [`InstallError::AlreadyManaged`]. Importing it would leave the user
    ///   with two copies of one skill; [`Installer::adopt`] is the operation
    ///   for that directory, and the message says so.
    /// - A name already taken in the store is [`InstallError::AlreadyExists`],
    ///   the same refusal a GitHub install gives, so the interface can offer
    ///   the same two answers: [`ImportOptions::named`] to keep both,
    ///   [`ImportOptions::replacing`] to replace. Replacing moves the old
    ///   directory to the trash first.
    ///
    /// # What is copied
    ///
    /// The tree, minus `.git`, which is the checkout's bookkeeping rather than
    /// part of the skill and is routinely larger than everything around it.
    /// Nothing is followed: a symlink inside the tree pointing back into the
    /// tree is recreated as written, and one pointing anywhere else is left
    /// behind rather than followed, so an import can only ever copy what is
    /// under the directory the user picked.
    ///
    /// A copy that fails part-way takes the half-written directory back out of
    /// the store, so a failed import leaves nothing an agent could load.
    pub fn import(
        &self,
        source: &Path,
        options: &ImportOptions,
    ) -> Result<(PathBuf, Outcome), InstallError> {
        let source = self.import_source(source)?;
        let name = match &options.name {
            Some(name) => name.clone(),
            None => import_name(&source),
        };
        if !is_kebab_case(&name) {
            return Err(InstallError::InvalidName { name });
        }

        let dest = self.ensure_in_scope(&self.roots.store_dir().join(&name))?;
        let mut outcome = Outcome::default();
        if fs::symlink_metadata(&dest).is_ok() {
            if !options.replace {
                return Err(InstallError::AlreadyExists { path: dest });
            }
            outcome
                .changes
                .extend(self.clear_for_import(&dest)?.changes);
        }
        create_dir_all(&self.roots.store_dir())?;

        if let Err(cause) = copy_tree(&source, &dest, CopyRule::Import { root: &source }) {
            let cause = InstallError::partial(outcome, cause);
            // The destination did not exist a moment ago and every byte in it
            // was written by the line above, so removing it destroys nothing
            // of the user's. Leaving it would leave a half-copied skill in the
            // directory every agent reads.
            return Err(match fs::remove_dir_all(&dest) {
                Ok(()) => cause,
                Err(e) => InstallError::Rollback {
                    cause: Box::new(cause),
                    undo: Box::new(InstallError::io(&dest, e)),
                    at: dest,
                    dangling_links: false,
                },
            });
        }

        outcome.push(Change::Copied {
            from: source,
            to: dest.clone(),
        });
        Ok((dest, outcome))
    }

    /// Checks that `source` is a directory holding a skill, and that it is not
    /// one Skillbase already manages. Returns it canonicalized.
    ///
    /// Canonicalizing first is what makes the scope check honest: a symlink in
    /// `~/Downloads` pointing at `~/.agents/skills/pdf` is that skill, and
    /// importing through it would make a second copy of it just the same.
    fn import_source(&self, source: &Path) -> Result<PathBuf, InstallError> {
        let source = fs::canonicalize(source).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => InstallError::NotASkill {
                path: source.to_path_buf(),
                hint: "There is nothing at that path. Check it in Finder, then try again.",
            },
            _ => InstallError::io(source, e),
        })?;
        if !fs::metadata(&source).map(|m| m.is_dir()).unwrap_or(false) {
            return Err(InstallError::NotASkill {
                path: source,
                hint: "A skill is a folder, so pick the folder that holds the SKILL.md rather \
                       than a file inside it.",
            });
        }
        if self.overlaps_managed(&source) {
            return Err(InstallError::AlreadyManaged { path: source });
        }
        match fs::read_to_string(source.join(SKILL_FILE_NAME)) {
            Ok(_) => Ok(source),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(InstallError::NotASkill {
                path: source,
                hint: "A skill is a folder with a SKILL.md in it. If you picked a repository, \
                       look one level down: its skills are usually in a folder like `skills`.",
            }),
            Err(e) => Err(InstallError::io(source.join(SKILL_FILE_NAME), e)),
        }
    }

    /// True when `path` and a directory Skillbase manages are the same
    /// directory, or either holds the other.
    ///
    /// Reads [`Roots::managed_roots`] rather than [`Roots::scope_roots`],
    /// because the question here is what the user already has, not where a
    /// write is allowed. The trash is the difference: a deleted skill is not
    /// in the list, so it cannot be adopted, and importing it back is the only
    /// way to get it back.
    ///
    /// Wider than [`Installer::ensure_in_scope`] in both directions.
    /// [`Installer::ensure_in_scope`] refuses a root itself, because a root is
    /// not a skill to be operated on; here the root counts, since importing
    /// `~/.claude/skills` is as much a second copy as importing one directory
    /// inside it. And a path *holding* a root counts too: a home directory
    /// with a stray `SKILL.md` in it is a directory, and copying it into the
    /// store would copy the store into itself.
    ///
    /// [`Roots::managed_roots`]: crate::registry::Roots::managed_roots
    /// [`Roots::scope_roots`]: crate::registry::Roots::scope_roots
    fn overlaps_managed(&self, path: &Path) -> bool {
        self.roots
            .managed_roots()
            .iter()
            .any(|root| path.starts_with(root) || root.starts_with(path))
    }

    /// Takes away what is already at `dest` before an import replaces it.
    ///
    /// A symlink is unlinked, because it holds no bytes of its own. Anything
    /// else goes to [`Installer::remove_real_dir`], which moves the directory
    /// into the trash: the skill being replaced may be one the user wrote, and
    /// a replace they did not mean has to be recoverable.
    fn clear_for_import(&self, dest: &Path) -> Result<Outcome, InstallError> {
        let meta = fs::symlink_metadata(dest).map_err(|e| InstallError::io(dest, e))?;
        if meta.file_type().is_symlink() {
            let target = fs::read_link(dest).ok();
            fs::remove_file(dest).map_err(|e| InstallError::io(dest, e))?;
            return Ok(Outcome::one(Change::RemovedSymlink {
                path: dest.to_path_buf(),
                target,
            }));
        }
        self.remove_real_dir(dest)
    }

    /// Renames a skill's directory, taking every symlink that pointed at it
    /// along.
    ///
    /// The directory keeps its parent, so a rename never changes whether a
    /// skill is shared, hidden or owned by an agent: only the last segment of
    /// the path changes. Returns the directory the skill now lives in.
    ///
    /// A skill's name is its frontmatter `name`, and discovery reads it from
    /// there rather than from the path. Renaming one is therefore two writes —
    /// this, and the `SKILL.md` the caller saves — and the two must both land
    /// or the skill ends up with a directory that disagrees with its own name,
    /// which is the state that sends an update into a second directory beside
    /// the first.
    ///
    /// Refuses a name that is not kebab-case before it touches anything, and a
    /// destination that is already occupied. A rename that fails part-way puts
    /// the directory and its links back; see [`Installer::move_origin`].
    pub fn rename(&self, from: &Path, new_name: &str) -> Result<(PathBuf, Outcome), InstallError> {
        if !is_kebab_case(new_name) {
            return Err(InstallError::InvalidName {
                name: new_name.to_string(),
            });
        }
        let from = self.ensure_in_scope(from)?;
        let parent = from
            .parent()
            .ok_or_else(|| InstallError::OutsideScope { path: from.clone() })?;
        let to = parent.join(new_name);
        if to == from {
            return Ok((
                from.clone(),
                Outcome::one(Change::NoChange {
                    path: from,
                    reason: "already the directory's name",
                }),
            ));
        }
        let outcome = self.move_origin(&from, &to)?;
        Ok((to, outcome))
    }

    /// Moves a skill's origin between two directories Skillbase owns, taking
    /// every symlink that pointed at it along.
    ///
    /// The links are collected before the move, because a link whose target has
    /// gone cannot be resolved afterwards, and repointed straight after it, so
    /// each one is broken only for the length of one rename.
    ///
    /// If a link cannot be repointed the origin is moved back and every link is
    /// put back, so a failure leaves the skill where it was. A link that will
    /// not go back does not stop the ones after it; see
    /// [`Installer::undo_move_origin`].
    fn move_origin(&self, from: &Path, to: &Path) -> Result<Outcome, InstallError> {
        let from = self.ensure_in_scope(from)?;
        let to = self.ensure_in_scope(to)?;
        require_dir(&from)?;
        if fs::symlink_metadata(&to).is_ok() {
            return Err(InstallError::AlreadyExists { path: to });
        }
        let links = self.links_pointing_at(&from);
        if let Some(parent) = to.parent() {
            create_dir_all(parent)?;
        }
        fs::rename(&from, &to).map_err(|e| InstallError::io(&from, e))?;

        let mut outcome = Outcome::one(Change::Moved {
            from: from.clone(),
            to: to.clone(),
        });
        for link in &links {
            match self.place_symlink(link, &to) {
                Ok(placed) => outcome.changes.extend(placed.changes),
                Err(cause) => {
                    return Err(self.undo_move_origin(&from, &to, &links, outcome, cause));
                }
            }
        }
        Ok(outcome)
    }

    /// Puts an origin and the links to it back where they were after a move
    /// failed part-way.
    ///
    /// Returns `cause` unchanged when everything went back. When the undo
    /// itself failed it returns an [`InstallError::Rollback`] naming both
    /// failures and where the skill's files ended up, because a message that
    /// reports only `cause` describes a state the disk is no longer in.
    ///
    /// `done` is what the move had managed before it stopped, and it is
    /// carried on the returned error in the one case where it still stands:
    /// the directory would not go back.
    fn undo_move_origin(
        &self,
        from: &Path,
        to: &Path,
        links: &[PathBuf],
        done: Outcome,
        cause: InstallError,
    ) -> InstallError {
        if let Err(e) = fs::rename(to, from) {
            // The directory is still at `to`, so the move and every link
            // already repointed at it stand.
            return InstallError::Rollback {
                cause: Box::new(InstallError::partial(done, cause)),
                undo: Box::new(InstallError::io(to, e)),
                at: to.to_path_buf(),
                dangling_links: false,
            };
        }
        // Every link is tried even after one has failed. The links past the
        // failure point at `to`, which no longer exists, and each one that
        // does go back is one fewer dangling link for the user to repair by
        // hand.
        let mut undo = None;
        for link in links {
            if let Err(e) = self.place_symlink(link, from)
                && undo.is_none()
            {
                undo = Some(e);
            }
        }
        match undo {
            // The directory went back, so `done` no longer describes the disk;
            // what stands is the links that would not follow it.
            Some(undo) => InstallError::Rollback {
                cause: Box::new(cause),
                undo: Box::new(undo),
                at: from.to_path_buf(),
                dangling_links: true,
            },
            None => cause,
        }
    }

    /// Every symlink in a directory Skillbase manages that resolves to
    /// `target`.
    ///
    /// The target written into each link is resolved textually against the
    /// link's own directory rather than through the filesystem, so this finds
    /// the links to an origin that has already been moved away as well as the
    /// links to one still in place.
    ///
    /// Reads only, and sorted, so the changes an operation reports come back in
    /// the same order every time.
    fn links_pointing_at(&self, target: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        // The managed roots, not the scope roots: a link in the trash was
        // deleted along with the skill it belonged to, and repointing it would
        // put a live link back into a directory nothing reads.
        for root in self.roots.managed_roots() {
            let Ok(entries) = fs::read_dir(&root) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                match fs::symlink_metadata(&path) {
                    Ok(meta) if meta.file_type().is_symlink() => {}
                    _ => continue,
                }
                let Ok(written) = fs::read_link(&path) else {
                    continue;
                };
                let resolved = if written.is_absolute() {
                    normalize_lexical(&written)
                } else {
                    normalize_lexical(&root.join(&written))
                };
                if resolved.as_deref() == Some(target) && !found.contains(&path) {
                    found.push(path);
                }
            }
        }
        found.sort();
        found
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
            let target = fs::read_link(dest).ok();
            fs::remove_file(dest).map_err(|e| InstallError::io(dest, e))?;
            return Ok(Outcome::one(Change::RemovedSymlink {
                path: dest.to_path_buf(),
                target,
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

    /// Takes away a directory that has been shown to be real, never a symlink,
    /// by moving it into the trash.
    ///
    /// Nothing here calls [`fs::remove_dir_all`] on a user's directory. A skill
    /// Skillbase did not download is the only copy of something a person wrote,
    /// and a confirmation cannot promise the delete is recoverable unless the
    /// delete actually is. The directory is renamed under
    /// [`Roots::trash_dir`], and the change reports where it landed.
    ///
    /// [`Roots::trash_dir`]: crate::registry::Roots::trash_dir
    pub(crate) fn remove_real_dir(&self, path: &Path) -> Result<Outcome, InstallError> {
        let path = self.ensure_in_scope(path)?;
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => Err(InstallError::NotALink {
                path,
                kind: "symlink",
            }),
            Ok(meta) if meta.is_dir() => self.move_to_trash(&path),
            Ok(_) => Err(InstallError::NotALink { path, kind: "file" }),
            Err(_) => Ok(Outcome::one(Change::NoChange {
                path,
                reason: "not there",
            })),
        }
    }

    /// Renames a directory into `~/.skillbase/trash/<name>-<unix timestamp>`.
    ///
    /// A rename is atomic and costs nothing however large the skill is. It only
    /// fails outright across a filesystem boundary — an agent directory on an
    /// external volume, say — and that is the one case that falls back to
    /// copying the tree and then removing the original. If that removal fails
    /// the error carries the copy, because otherwise the user is left with the
    /// same skill in two places and nothing that says so.
    fn move_to_trash(&self, path: &Path) -> Result<Outcome, InstallError> {
        let trash = self.roots.trash_dir();
        create_dir_all(&trash)?;
        let dest = self.ensure_in_scope(&trash.join(trash_name(path, &trash)))?;

        if let Err(rename_error) = fs::rename(path, &dest) {
            if !crosses_filesystems(&rename_error) {
                return Err(InstallError::io(path, rename_error));
            }
            copy_dir(path, &dest)?;
            if let Err(e) = fs::remove_dir_all(path) {
                // The copy is in the trash and the original is still where it
                // was. Reporting the copy is what keeps the user from a
                // duplicate nothing told them about.
                return Err(InstallError::partial(
                    Outcome::one(Change::Copied {
                        from: path.to_path_buf(),
                        to: dest.clone(),
                    }),
                    InstallError::io(path, e),
                ));
            }
        }
        Ok(Outcome::one(Change::MovedToTrash {
            from: path.to_path_buf(),
            to: dest,
        }))
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
            // What the old link said, as written, rather than the resolved
            // target above: a restore rewrites the link exactly as it was.
            let written = fs::read_link(&from).ok();
            let mut outcome = self.place_symlink(&to, &target)?;
            fs::remove_file(&from).map_err(|e| InstallError::io(&from, e))?;
            outcome.push(Change::RemovedSymlink {
                path: from,
                target: written,
            });
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

/// True when `path` is a directory in its own right, not a symlink to one.
fn is_real_dir(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(meta) => !meta.file_type().is_symlink() && meta.is_dir(),
        Err(_) => false,
    }
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

/// The name a directory takes in the trash: `<name>-<unix timestamp>`.
///
/// The timestamp is what tells one delete of `code-review` from the next. It is
/// not enough on its own: deleting a skill that was copied into eight agent
/// directories moves nine directories with the same name within one second, so
/// a name already taken gets a counter as well.
fn trash_name(path: &Path, trash: &Path) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "skill".to_string());
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let stamped = format!("{name}-{seconds}");
    if fs::symlink_metadata(trash.join(&stamped)).is_err() {
        return stamped;
    }
    for n in 2u32.. {
        let candidate = format!("{stamped}-{n}");
        if fs::symlink_metadata(trash.join(&candidate)).is_err() {
            return candidate;
        }
    }
    stamped
}

/// True when a rename failed because the two paths are on different
/// filesystems, which is the one failure a copy can still get past.
pub(crate) fn crosses_filesystems(error: &std::io::Error) -> bool {
    // EXDEV, 18 on every platform this runs on. Matched by number as well as by
    // kind because the kind was only classified recently and an older libc
    // still reports it as `Uncategorized`.
    error.kind() == std::io::ErrorKind::CrossesDevices || error.raw_os_error() == Some(18)
}

/// The name a checkout keeps its history under, which an import leaves behind.
const GIT_DIR: &str = ".git";

/// What a copy takes with it.
#[derive(Debug, Clone, Copy)]
enum CopyRule<'a> {
    /// Every entry, symlinks recreated exactly as they were written. Used for
    /// directories that are already inside a managed scope, where the tree is
    /// one Skillbase or an agent put there.
    Everything,
    /// A skill being imported from outside every scope, whose tree is the
    /// user's own and may hold anything: `.git` and [`COPY_MARKER`] are left
    /// behind, and so is a symlink that points out of `root`, so a copy can
    /// only ever take what is under the directory the user picked.
    Import {
        /// The canonicalized root of the tree being copied.
        root: &'a Path,
    },
}

impl CopyRule<'_> {
    /// True when an entry of this name is not copied at all.
    fn skips(&self, name: &std::ffi::OsStr) -> bool {
        match self {
            Self::Everything => false,
            Self::Import { .. } => name == GIT_DIR || name == COPY_MARKER,
        }
    }

    /// True when a symlink at `at`, written as `target`, is copied.
    ///
    /// Never followed either way: this decides between recreating the link and
    /// leaving it out.
    fn keeps_link(&self, at: &Path, target: &Path) -> bool {
        let root = match self {
            Self::Everything => return true,
            Self::Import { root } => root,
        };
        let absolute = if target.is_absolute() {
            target.to_path_buf()
        } else {
            match at.parent() {
                Some(parent) => parent.join(target),
                None => return false,
            }
        };
        normalize_lexical(&absolute).is_some_and(|resolved| resolved.starts_with(root))
    }
}

/// Copies a directory tree, following nothing.
pub(crate) fn copy_dir(from: &Path, to: &Path) -> Result<(), InstallError> {
    copy_tree(from, to, CopyRule::Everything)
}

/// Copies a directory tree, following nothing and taking the entries `rule`
/// allows.
fn copy_tree(from: &Path, to: &Path, rule: CopyRule<'_>) -> Result<(), InstallError> {
    create_dir_all(to)?;
    let entries = fs::read_dir(from).map_err(|e| InstallError::io(from, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| InstallError::io(from, e))?;
        if rule.skips(&entry.file_name()) {
            continue;
        }
        let source = entry.path();
        let target = to.join(entry.file_name());
        let meta = fs::symlink_metadata(&source).map_err(|e| InstallError::io(&source, e))?;
        if meta.is_dir() {
            copy_tree(&source, &target, rule)?;
        } else if meta.file_type().is_symlink() {
            let link = fs::read_link(&source).map_err(|e| InstallError::io(&source, e))?;
            if !rule.keeps_link(&source, &link) {
                continue;
            }
            symlink(&link, &target).map_err(|e| InstallError::io(&target, e))?;
        } else {
            fs::copy(&source, &target).map_err(|e| InstallError::io(&source, e))?;
        }
    }
    Ok(())
}

/// The name an imported directory takes: the one discovery would show it under,
/// in the shape a directory name has to be.
///
/// The frontmatter `name` wins, because that is the name the agents use and the
/// name the user will look for. It is slugified when it is not already
/// kebab-case — `My Skill` becomes `my-skill` rather than being refused — and
/// the directory's own name stands in when the frontmatter has no usable one,
/// which is what discovery falls back to as well.
fn import_name(source: &Path) -> String {
    let frontmatter = fs::read_to_string(source.join(SKILL_FILE_NAME))
        .ok()
        .and_then(|text| SkillDoc::parse(&text).ok())
        .and_then(|doc| doc.frontmatter.name().map(str::to_owned))
        .filter(|name| !name.trim().is_empty());
    match frontmatter {
        Some(name) if is_kebab_case(&name) => name,
        Some(name) => slugify(&name),
        None => slugify(&source.file_name().unwrap_or_default().to_string_lossy()),
    }
}

/// What a single path in a skill directory is, for the purpose of comparing
/// two of them.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EntryKind {
    Dir,
    File,
    /// A symlink, compared by the target it was written with rather than by
    /// what the target holds.
    Link(PathBuf),
    /// Something that could not be read. Never equal to anything, including
    /// another unreadable entry, so an unreadable duplicate is reported as
    /// divergent rather than quietly replaced.
    Unreadable,
}

/// Compares two skill directories file by file.
///
/// Follows nothing: a symlink inside either tree is compared by its target as
/// written. [`COPY_MARKER`] is ignored, because it is Skillbase's own
/// bookkeeping rather than part of the skill.
fn compare_dirs(origin: &Path, other: &Path) -> ContentDiff {
    let mut left = BTreeMap::new();
    let mut right = BTreeMap::new();
    walk_entries(origin, Path::new(""), &mut left);
    walk_entries(other, Path::new(""), &mut right);

    let mut diff = ContentDiff::default();
    for (relative, kind) in &left {
        match right.get(relative) {
            None => diff.missing.push(relative.clone()),
            Some(theirs) if theirs != kind || *kind == EntryKind::Unreadable => {
                diff.differing.push(relative.clone())
            }
            Some(_) if *kind == EntryKind::File => {
                if !files_equal(&origin.join(relative), &other.join(relative)) {
                    diff.differing.push(relative.clone());
                }
            }
            Some(_) => {}
        }
    }
    for relative in right.keys() {
        if !left.contains_key(relative) {
            diff.extra.push(relative.clone());
        }
    }
    diff
}

/// Records every path under `dir` by its path relative to the skill root.
fn walk_entries(dir: &Path, prefix: &Path, out: &mut BTreeMap<PathBuf, EntryKind>) {
    let Ok(entries) = fs::read_dir(dir) else {
        // An unreadable directory is a difference, not an equality: record it
        // under its own name so the comparison can never call it identical.
        if !prefix.as_os_str().is_empty() {
            out.insert(prefix.to_path_buf(), EntryKind::Unreadable);
        }
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == COPY_MARKER {
            continue;
        }
        let relative = prefix.join(&name);
        let path = entry.path();
        let kind = match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                EntryKind::Link(fs::read_link(&path).unwrap_or_default())
            }
            Ok(meta) if meta.is_dir() => EntryKind::Dir,
            Ok(_) => EntryKind::File,
            Err(_) => EntryKind::Unreadable,
        };
        let recurse = kind == EntryKind::Dir;
        out.insert(relative.clone(), kind);
        if recurse {
            walk_entries(&path, &relative, out);
        }
    }
}

/// True when two files hold the same bytes.
///
/// Compares in chunks so that a large bundled asset does not have to be held in
/// memory twice. A file that cannot be read is never equal to anything.
fn files_equal(left: &Path, right: &Path) -> bool {
    let (Ok(left_meta), Ok(right_meta)) = (fs::metadata(left), fs::metadata(right)) else {
        return false;
    };
    if left_meta.len() != right_meta.len() {
        return false;
    }
    let (Ok(left_file), Ok(right_file)) = (fs::File::open(left), fs::File::open(right)) else {
        return false;
    };
    let mut left_reader = BufReader::new(left_file);
    let mut right_reader = BufReader::new(right_file);
    loop {
        let (Ok(left_chunk), Ok(right_chunk)) = (left_reader.fill_buf(), right_reader.fill_buf())
        else {
            return false;
        };
        if left_chunk.is_empty() || right_chunk.is_empty() {
            return left_chunk.is_empty() && right_chunk.is_empty();
        }
        let len = left_chunk.len().min(right_chunk.len());
        if left_chunk[..len] != right_chunk[..len] {
            return false;
        }
        left_reader.consume(len);
        right_reader.consume(len);
    }
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
    use crate::discovery::{Location, LocationKind};
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
            fx.private(),
            fx.private().join("../.."),
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
        assert_eq!(
            installer.ensure_in_scope(&fx.private().join("a")).unwrap(),
            fx.private().join("a"),
            "the private directory is Skillbase's to write in"
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
                path: fx.agent("claude-code").join("shared-one"),
                target: Some(PathBuf::from("../../.agents/skills/shared-one")),
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

    // -- the shared switch ---------------------------------------------------

    fn shared() -> &'static AgentDef {
        Registry::shared()
    }

    #[test]
    fn linking_shared_does_nothing_when_the_origin_is_already_in_the_store() {
        let fx = Fixture::realistic();
        let before = listing(fx.home());
        let origin = fx.store().join("shared-one");

        let outcome = fx
            .installer()
            .link("shared-one", &origin, shared())
            .unwrap();
        assert_eq!(
            outcome.changes,
            [Change::NoChange {
                path: origin.clone(),
                reason: "in the shared directory",
            }]
        );
        assert_eq!(before, listing(fx.home()), "nothing was written");
        assert!(
            !fs::symlink_metadata(&origin)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the shared directory is never linked into itself"
        );

        // The same through the higher-level call the switch makes.
        assert!(
            fx.installer()
                .share("shared-one", &origin)
                .unwrap()
                .is_noop()
        );
        assert_eq!(before, listing(fx.home()));
    }

    #[test]
    fn turning_shared_off_moves_the_origin_into_the_private_directory() {
        let fx = Fixture::realistic();
        let origin = fx.store().join("shared-one");
        let hidden = fx.private().join("shared-one");

        let outcome = fx.installer().unshare("shared-one", &origin).unwrap();
        assert_eq!(
            outcome.changes[0],
            Change::Moved {
                from: origin.clone(),
                to: hidden.clone(),
            }
        );
        assert!(!origin.exists(), "the agents no longer see it");
        assert!(hidden.join(SKILL_FILE_NAME).is_file());

        let result = fx.scan();
        let skill = result.get("shared-one").unwrap();
        assert_eq!(skill.origin, hidden);
        assert!(skill.managed, "the private directory is Skillbase's too");
        assert!(!skill.is_present_in("shared"));
        assert!(!skill.visible_to().contains(&"cursor"));
        assert!(
            skill.visible_to().contains(&"claude-code"),
            "the agent that holds its own link keeps the skill"
        );
    }

    #[test]
    fn turning_shared_off_repoints_every_link_that_pointed_at_the_origin() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        // A second link, and one parked in a disabled directory, so the repoint
        // has to cover more than the one agent the fixture starts with.
        installer
            .link("shared-one", &fx.store().join("shared-one"), cursor())
            .unwrap();
        fx.link_rel(
            ".claude/skills-disabled/shared-one",
            "../../.agents/skills/shared-one",
        );

        installer
            .unshare("shared-one", &fx.store().join("shared-one"))
            .unwrap();

        for link in [
            fx.agent("claude-code").join("shared-one"),
            fx.agent("cursor").join("shared-one"),
            fx.home().join(".claude/skills-disabled/shared-one"),
        ] {
            assert!(
                fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{} is still a link",
                link.display()
            );
            assert_eq!(
                fs::canonicalize(&link).unwrap(),
                fx.private().join("shared-one"),
                "{} follows the origin",
                link.display()
            );
            assert!(
                link.join(SKILL_FILE_NAME).is_file(),
                "{} still resolves",
                link.display()
            );
        }
        assert_eq!(
            fs::read_link(fx.agent("cursor").join("shared-one")).unwrap(),
            Path::new("../../.skillbase/private/shared-one"),
            "and the new target is relative, like every other link"
        );
    }

    #[test]
    fn turning_shared_on_moves_the_origin_back_and_repoints_the_links() {
        let fx = Fixture::realistic();
        let hidden = fx.private().join("hidden-one");
        let link = fx.agent("claude-code").join("hidden-one");

        let outcome = fx.installer().share("hidden-one", &hidden).unwrap();
        assert_eq!(
            outcome.changes[0],
            Change::Moved {
                from: hidden.clone(),
                to: fx.store().join("hidden-one"),
            }
        );
        assert!(!hidden.exists());
        assert_eq!(
            fs::read_link(&link).unwrap(),
            Path::new("../../.agents/skills/hidden-one")
        );
        assert!(link.join(SKILL_FILE_NAME).is_file());

        let result = fx.scan();
        let skill = result.get("hidden-one").unwrap();
        assert_eq!(skill.origin, fx.store().join("hidden-one"));
        assert!(skill.managed);
        assert!(skill.is_present_in("shared"));
        assert!(skill.visible_to().contains(&"cursor"));
        assert!(skill.visible_to().contains(&"claude-code"));
    }

    #[test]
    fn turning_shared_off_and_on_again_leaves_the_tree_as_it_was() {
        let fx = Fixture::realistic();
        let before = listing(fx.home());
        let installer = fx.installer();

        installer
            .unshare("shared-one", &fx.store().join("shared-one"))
            .unwrap();
        assert_ne!(before, listing(fx.home()));

        installer
            .share("shared-one", &fx.private().join("shared-one"))
            .unwrap();
        assert_eq!(
            before,
            listing(fx.home()),
            "the origin and every link are back where they were"
        );
    }

    #[test]
    fn the_switch_reaches_the_move_through_link_and_unlink_as_well() {
        let fx = Fixture::realistic();
        let installer = fx.installer();

        // Off: `unlink` finds a real directory in the shared scope, which is the
        // origin, and moves it rather than refusing.
        installer.unlink("shared-one", shared()).unwrap();
        assert_eq!(
            fx.scan().get("shared-one").unwrap().origin,
            fx.private().join("shared-one")
        );

        // On again: `link` moves it back.
        installer
            .link("shared-one", &fx.private().join("shared-one"), shared())
            .unwrap();
        assert_eq!(
            fx.scan().get("shared-one").unwrap().origin,
            fx.store().join("shared-one")
        );
    }

    #[test]
    fn hiding_a_skill_that_is_already_hidden_does_nothing() {
        let fx = Fixture::realistic();
        let before = listing(fx.home());
        let outcome = fx
            .installer()
            .unshare("hidden-one", &fx.private().join("hidden-one"))
            .unwrap();
        assert!(outcome.is_noop());
        assert_eq!(before, listing(fx.home()));
    }

    #[test]
    fn the_shared_switch_on_an_unmanaged_skill_writes_a_link_and_removes_it() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        let origin = fx.agent("claude-code").join("claude-only");

        installer.share("claude-only", &origin).unwrap();
        let link = fx.shared().join("claude-only");
        assert_eq!(
            fs::read_link(&link).unwrap(),
            Path::new("../../.claude/skills/claude-only"),
            "another tool's directory is linked, not moved"
        );
        assert!(
            fx.scan()
                .get("claude-only")
                .unwrap()
                .visible_to()
                .contains(&"cursor")
        );

        installer.unshare("claude-only", &origin).unwrap();
        assert!(!link.exists());
        assert!(
            origin.join(SKILL_FILE_NAME).is_file(),
            "and the directory it pointed at is untouched"
        );
    }

    #[test]
    fn the_shared_switch_refuses_a_move_onto_a_name_that_is_taken() {
        let fx = Fixture::realistic();
        fx.skill(".skillbase/private/shared-one", "shared-one-hidden");
        let before = listing(fx.home());

        let err = fx
            .installer()
            .unshare("shared-one", &fx.store().join("shared-one"))
            .unwrap_err();
        assert!(matches!(err, InstallError::AlreadyExists { .. }));
        assert_eq!(before, listing(fx.home()), "a refusal changes nothing");
    }

    // -- adopt and release --------------------------------------------------

    #[test]
    fn adopt_moves_the_origin_into_the_store_and_leaves_a_link() {
        let fx = Fixture::realistic();
        let origin = fx.agent("claude-code").join("claude-only");
        let outcome = fx.installer().adopt("claude-only", &origin).unwrap();

        assert_eq!(
            outcome.changes,
            [
                Change::Moved {
                    from: origin.clone(),
                    to: fx.store().join("claude-only"),
                },
                Change::CreatedSymlink {
                    path: origin.clone(),
                    target: PathBuf::from("../../.agents/skills/claude-only"),
                },
            ]
        );
        assert!(fx.store().join("claude-only/SKILL.md").is_file());
        assert!(origin.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(
            origin.join(SKILL_FILE_NAME).is_file(),
            "the link Claude Code is left with still resolves"
        );

        let skill = fx.scan();
        let adopted = skill.get("claude-only").unwrap();
        assert!(adopted.managed);
        assert_eq!(adopted.origin, fx.store().join("claude-only"));
        assert!(
            adopted.visible_to().contains(&"cursor"),
            "adopting a skill puts it in the directory the agents read"
        );
    }

    #[test]
    fn adopt_and_release_round_trip() {
        let fx = Fixture::realistic();
        let before = listing(fx.home());
        let origin = fx.agent("claude-code").join("claude-only");
        let installer = fx.installer();

        installer.adopt("claude-only", &origin).unwrap();
        let outcome = installer.release("claude-only", &origin).unwrap();
        assert_eq!(
            outcome.changes,
            [
                Change::RemovedSymlink {
                    path: origin.clone(),
                    target: Some(PathBuf::from("../../.agents/skills/claude-only")),
                },
                Change::Moved {
                    from: fx.store().join("claude-only"),
                    to: origin,
                },
            ]
        );
        assert_eq!(before, listing(fx.home()), "release undoes adopt exactly");
    }

    #[test]
    fn adopt_refuses_when_the_store_already_has_that_name() {
        let fx = Fixture::realistic();
        fx.skill(".agents/skills/claude-only", "claude-only");
        let before = listing(fx.home());
        let err = fx
            .installer()
            .adopt("claude-only", &fx.agent("claude-code").join("claude-only"))
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
    fn adopting_a_hidden_skill_shares_it_rather_than_moving_it_twice() {
        let fx = Fixture::realistic();
        let outcome = fx
            .installer()
            .adopt("hidden-one", &fx.private().join("hidden-one"))
            .unwrap();

        assert_eq!(
            outcome.changes[0],
            Change::Moved {
                from: fx.private().join("hidden-one"),
                to: fx.store().join("hidden-one"),
            }
        );
        assert!(
            !fx.private().join("hidden-one").exists(),
            "no symlink is left behind in the private directory"
        );
        assert!(fx.store().join("hidden-one/SKILL.md").is_file());
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

    // -- rename ---------------------------------------------------------------

    #[test]
    fn rename_moves_the_directory_and_repoints_every_link_to_it() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        let origin = fx.store().join("shared-one");
        // Two links and one parked in a disabled directory, so the repoint has
        // to cover more than the one agent the fixture starts with.
        installer.link("shared-one", &origin, cursor()).unwrap();
        fx.link_rel(
            ".claude/skills-disabled/shared-one",
            "../../.agents/skills/shared-one",
        );

        let (to, outcome) = installer.rename(&origin, "shared-uno").unwrap();

        assert_eq!(to, fx.store().join("shared-uno"));
        assert_eq!(
            outcome.changes[0],
            Change::Moved {
                from: origin.clone(),
                to: to.clone(),
            }
        );
        assert!(!origin.exists(), "the old directory is gone");
        assert!(to.join(SKILL_FILE_NAME).is_file());
        // The links keep their old names — they are named for the skill the
        // frontmatter has not been rewritten to yet — and they resolve to the
        // directory under its new one.
        for link in [
            fx.agent("claude-code").join("shared-one"),
            fx.agent("cursor").join("shared-one"),
            fx.home().join(".claude/skills-disabled/shared-one"),
        ] {
            assert_eq!(
                fs::canonicalize(&link).unwrap(),
                to,
                "{} follows the directory",
                link.display()
            );
        }
        assert_eq!(
            fs::read_link(fx.agent("cursor").join("shared-one")).unwrap(),
            Path::new("../../.agents/skills/shared-uno"),
            "and the new target is relative, like every other link"
        );
    }

    #[test]
    fn rename_keeps_the_parent_so_a_hidden_skill_stays_hidden() {
        let fx = Fixture::realistic();
        let (to, _) = fx
            .installer()
            .rename(&fx.private().join("hidden-one"), "hidden-uno")
            .unwrap();

        assert_eq!(to, fx.private().join("hidden-uno"));
        assert!(
            !fx.store().join("hidden-uno").exists(),
            "renaming does not share a hidden skill"
        );
    }

    #[test]
    fn rename_refuses_a_name_that_is_not_kebab_case_and_changes_nothing() {
        let fx = Fixture::realistic();
        let before = listing(fx.home());

        for name in ["Shared One", "shared_one", "", "-shared", "shared/one"] {
            assert!(
                matches!(
                    fx.installer().rename(&fx.store().join("shared-one"), name),
                    Err(InstallError::InvalidName { .. })
                ),
                "{name} should have been refused"
            );
        }
        assert_eq!(before, listing(fx.home()), "a refusal changes nothing");
    }

    #[test]
    fn rename_refuses_a_name_another_directory_already_holds() {
        let fx = Fixture::realistic();
        let before = listing(fx.home());

        let err = fx
            .installer()
            .rename(&fx.store().join("shared-one"), "shared-two")
            .unwrap_err();

        assert!(matches!(err, InstallError::AlreadyExists { .. }));
        assert_eq!(before, listing(fx.home()), "a refusal changes nothing");
    }

    #[test]
    fn renaming_to_the_name_it_already_has_does_nothing() {
        let fx = Fixture::realistic();
        let before = listing(fx.home());

        let (to, outcome) = fx
            .installer()
            .rename(&fx.store().join("shared-one"), "shared-one")
            .unwrap();

        assert_eq!(to, fx.store().join("shared-one"));
        assert!(outcome.is_noop());
        assert_eq!(before, listing(fx.home()));
    }

    #[test]
    fn rename_refuses_a_path_outside_the_scopes() {
        let fx = Fixture::realistic();
        let outside = fx.home().join("Documents/secret");

        assert!(matches!(
            fx.installer().rename(&outside, "not-a-secret"),
            Err(InstallError::OutsideScope { .. })
        ));
        assert!(outside.is_dir());
    }

    #[test]
    fn rename_and_rename_back_leaves_the_tree_as_it_was() {
        let fx = Fixture::realistic();
        let before = listing(fx.home());
        let installer = fx.installer();

        let (to, _) = installer
            .rename(&fx.store().join("shared-one"), "shared-uno")
            .unwrap();
        assert_ne!(before, listing(fx.home()));

        installer.rename(&to, "shared-one").unwrap();
        assert_eq!(before, listing(fx.home()));
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
        assert_eq!(
            plan.link_count(),
            0,
            "the store is the shared directory, so nothing links to it"
        );
        assert_eq!(plan.total(), 1);

        let plan = installer.plan_delete(result.get("hidden-one").unwrap());
        assert_eq!(plan.origin, Some(fx.private().join("hidden-one")));
        assert_eq!(plan.link_count(), 1, "Claude Code links to the origin");
        assert_eq!(plan.total(), 2);

        let plan = installer.plan_delete(result.get("disabled-here").unwrap());
        assert_eq!(plan.link_count(), 1, "a parked link still counts");
    }

    /// Both leave `origin` empty, and a caller that has to know whether the
    /// skill's bytes are still somewhere cannot tell them apart from that.
    #[test]
    fn plan_delete_separates_an_origin_that_is_gone_from_one_out_of_scope() {
        let fx = Fixture::realistic();
        // A skill Claude Code reaches through a link into a directory Skillbase
        // does not manage, so discovery resolves its origin outside every
        // scope.
        fx.skill("Documents/skills/outside-one", "outside-one");
        fx.link_rel(
            ".claude/skills/outside-one",
            "../../Documents/skills/outside-one",
        );
        let installer = fx.installer();
        let scan = fx.scan();

        let plan = installer.plan_delete(scan.get("shared-one").unwrap());
        assert_eq!(plan.origin, Some(fx.shared().join("shared-one")));
        assert!(!plan.origin_missing);

        // Outside every scope: the directory is untouched and still there, so
        // the plan lists it as left alone rather than as gone.
        let outside = scan.get("outside-one").unwrap();
        let plan = installer.plan_delete(outside);
        assert_eq!(plan.origin, None);
        assert!(!plan.origin_missing);
        assert!(plan.skipped.contains(&outside.origin));

        // Removed between the scan and the plan.
        fs::remove_dir_all(fx.shared().join("shared-one")).unwrap();
        let plan = installer.plan_delete(scan.get("shared-one").unwrap());
        assert_eq!(plan.origin, None);
        assert!(plan.origin_missing);
        assert!(
            plan.skipped.is_empty(),
            "the origin was in scope, so nothing was left alone for being outside one"
        );
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

    // -- delete is recoverable -------------------------------------------------

    /// The one entry in the trash, as (name, path).
    fn trashed(fx: &Fixture) -> Vec<(String, PathBuf)> {
        let mut found: Vec<_> = fs::read_dir(fx.roots().trash_dir())
            .expect("the trash directory")
            .flatten()
            .map(|entry| {
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    entry.path(),
                )
            })
            .collect();
        found.sort();
        found
    }

    /// A listing with the trash left out.
    ///
    /// A delete creates `~/.skillbase/trash` and a restore does not take the
    /// now-empty directory away again, so that entry is the only difference a
    /// full round trip is allowed to leave behind.
    fn without_trash(entries: &[String]) -> Vec<String> {
        entries
            .iter()
            .filter(|line| !line.starts_with(".skillbase/trash"))
            .cloned()
            .collect()
    }

    #[test]
    fn delete_moves_the_origin_to_the_trash_instead_of_destroying_it() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        let plan = installer.plan_delete(fx.scan().get("shared-one").unwrap());

        let outcome = installer.delete(&plan).unwrap();

        assert!(
            !fx.shared().join("shared-one").exists(),
            "gone from the store"
        );
        let entries = trashed(&fx);
        assert_eq!(entries.len(), 1);
        let (name, path) = &entries[0];
        assert!(
            name.starts_with("shared-one-"),
            "the trashed name carries the skill's name: {name}"
        );
        assert!(
            name.trim_start_matches("shared-one-")
                .chars()
                .all(|c| c.is_ascii_digit()),
            "and a unix timestamp: {name}"
        );
        assert!(
            fs::read_to_string(path.join(SKILL_FILE_NAME))
                .unwrap()
                .contains("shared-one"),
            "the bytes are still there to be got back"
        );
        assert!(
            outcome.changes.contains(&Change::MovedToTrash {
                from: fx.shared().join("shared-one"),
                to: path.clone(),
            }),
            "and the outcome names where it went: {:?}",
            outcome.changes
        );
        assert!(
            outcome
                .describe_under(fx.home())
                .contains("moved ~/.agents/skills/shared-one to the trash at ~/.skillbase/trash/"),
            "{}",
            outcome.describe_under(fx.home())
        );
    }

    #[test]
    fn deleting_two_directories_of_the_same_name_keeps_both_in_the_trash() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        // `copied-around` is the origin in the store plus one real copy under
        // Gemini: one delete, two directories of the same name, one second.
        let plan = installer.plan_delete(fx.scan().get("copied-around").unwrap());
        assert_eq!(plan.copy_count(), 1);

        installer.delete(&plan).unwrap();

        let entries = trashed(&fx);
        assert_eq!(
            entries.len(),
            2,
            "neither one overwrote the other: {entries:?}"
        );
        for (_, path) in &entries {
            assert!(path.join(SKILL_FILE_NAME).is_file());
        }
    }

    #[test]
    fn a_deleted_skill_can_be_imported_back_out_of_the_trash() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        let plan = installer.plan_delete(fx.scan().get("shared-one").unwrap());
        installer.delete(&plan).unwrap();
        let entries = trashed(&fx);
        assert_eq!(entries.len(), 1);
        let (_, trashed_dir) = &entries[0];

        // The delete told the user the skill can be got back, and this is the
        // operation that gets it back: the trash is not a directory Skillbase
        // manages, so importing out of it is an import like any other.
        let (dir, outcome) = installer
            .import(trashed_dir, &ImportOptions::new())
            .expect("a trashed directory imports");

        assert_eq!(dir, fx.store().join("shared-one"));
        assert_eq!(
            outcome.changes,
            vec![Change::Copied {
                from: trashed_dir.clone(),
                to: dir.clone(),
            }]
        );
        assert!(
            fx.scan().get("shared-one").is_some(),
            "it is back in the list"
        );
        assert!(
            trashed_dir.join(SKILL_FILE_NAME).is_file(),
            "an import copies, so the trashed directory is still there"
        );
    }

    #[test]
    fn a_deleted_skill_does_not_come_back_in_a_scan() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        let plan = installer.plan_delete(fx.scan().get("shared-one").unwrap());
        installer.delete(&plan).unwrap();
        assert!(
            fx.scan().get("shared-one").is_none(),
            "the trash is a scope Skillbase writes to, never one it reads"
        );
    }

    // -- undoing a delete ------------------------------------------------

    #[test]
    fn restore_puts_the_origin_and_every_link_back_where_they_were() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        let before = listing(fx.home());
        let plan = installer.plan_delete(fx.scan().get("shared-one").unwrap());
        let deleted = installer.delete(&plan).unwrap();

        let restored = installer.restore(&deleted.changes);

        assert!(!restored.is_noop());
        assert_eq!(restored.directories(), 1);
        assert_eq!(restored.links(), 1);
        assert!(restored.missed.is_empty(), "{:?}", restored.missed);
        assert!(
            fs::read_to_string(fx.shared().join("shared-one").join(SKILL_FILE_NAME))
                .unwrap()
                .contains("shared-one"),
            "the files came back out of the trash with their bytes"
        );
        assert_eq!(
            fs::read_link(fx.agent("claude-code").join("shared-one")).unwrap(),
            PathBuf::from("../../.agents/skills/shared-one"),
            "and the link points where it pointed"
        );
        assert_eq!(
            without_trash(&before),
            without_trash(&listing(fx.home())),
            "the home is as it was before the delete"
        );
        assert!(
            fx.scan().get("shared-one").is_some(),
            "it is back in the list"
        );

        let described = restored.describe_under(fx.home());
        assert!(
            described.contains("to ~/.agents/skills/shared-one"),
            "{described}"
        );
        assert!(
            described
                .contains("linked ~/.claude/skills/shared-one -> ../../.agents/skills/shared-one"),
            "{described}"
        );
    }

    #[test]
    fn restore_brings_a_trashed_duplicate_directory_back_too() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        // `copied-around` is the origin in the store plus one real copy under
        // Gemini, so the delete trashes two directories.
        let plan = installer.plan_delete(fx.scan().get("copied-around").unwrap());
        assert_eq!(plan.copy_count(), 1);
        let deleted = installer.delete(&plan).unwrap();
        assert!(!fx.agent("gemini-cli").join("copied-around").exists());

        let restored = installer.restore(&deleted.changes);

        assert_eq!(restored.directories(), 2);
        assert!(restored.missed.is_empty(), "{:?}", restored.missed);
        for dir in [
            fx.shared().join("copied-around"),
            fx.agent("gemini-cli").join("copied-around"),
        ] {
            assert!(
                dir.join(SKILL_FILE_NAME).is_file(),
                "{} came back",
                dir.display()
            );
        }
        assert!(
            trashed(&fx).is_empty(),
            "and nothing was left behind in the trash: {:?}",
            trashed(&fx)
        );
    }

    #[test]
    fn a_removed_link_records_what_it_pointed_at() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        let plan = installer.plan_delete(fx.scan().get("hidden-one").unwrap());

        let outcome = installer.delete(&plan).unwrap();

        assert!(
            outcome.changes.contains(&Change::RemovedSymlink {
                path: fx.agent("claude-code").join("hidden-one"),
                target: Some(PathBuf::from("../../.skillbase/private/hidden-one")),
            }),
            "the target is kept as it was written: {:?}",
            outcome.changes
        );
    }

    #[test]
    fn restore_leaves_a_link_path_something_else_now_holds_alone() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        let plan = installer.plan_delete(fx.scan().get("shared-one").unwrap());
        let deleted = installer.delete(&plan).unwrap();
        // The user has written a skill of their own where the link was.
        let taken = fx.skill(".claude/skills/shared-one", "written-since");

        let restored = installer.restore(&deleted.changes);

        assert_eq!(restored.links(), 0);
        assert_eq!(
            restored.missed,
            vec![Missed {
                path: taken.clone(),
                reason: MissReason::Occupied,
            }]
        );
        assert!(
            fs::read_to_string(taken.join(SKILL_FILE_NAME))
                .unwrap()
                .contains("written-since"),
            "what was there is untouched"
        );
        assert!(
            restored.describe_under(fx.home()).contains(
                "could not put back ~/.claude/skills/shared-one: something else is there now"
            ),
            "{}",
            restored.describe_under(fx.home())
        );
    }

    #[test]
    fn restore_leaves_a_directory_whose_old_path_is_taken_alone() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        let plan = installer.plan_delete(fx.scan().get("shared-one").unwrap());
        let deleted = installer.delete(&plan).unwrap();
        let taken = fx.skill(".agents/skills/shared-one", "written-since");

        let restored = installer.restore(&deleted.changes);

        assert_eq!(restored.directories(), 0);
        assert!(
            restored.missed.contains(&Missed {
                path: taken.clone(),
                reason: MissReason::Occupied,
            }),
            "{:?}",
            restored.missed
        );
        assert!(
            fs::read_to_string(taken.join(SKILL_FILE_NAME))
                .unwrap()
                .contains("written-since"),
            "the skill written since is untouched"
        );
        assert_eq!(
            trashed(&fx).len(),
            1,
            "and what the delete took is still in the trash to be got back by hand"
        );
    }

    #[test]
    fn restore_reports_a_directory_the_user_has_emptied_out_of_the_trash() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        let plan = installer.plan_delete(fx.scan().get("shared-one").unwrap());
        let deleted = installer.delete(&plan).unwrap();
        fs::remove_dir_all(fx.roots().trash_dir()).unwrap();

        let restored = installer.restore(&deleted.changes);

        assert_eq!(restored.directories(), 0);
        assert!(
            restored.missed.contains(&Missed {
                path: fx.shared().join("shared-one"),
                reason: MissReason::Gone,
            }),
            "{:?}",
            restored.missed
        );
        assert!(
            restored.describe_under(fx.home()).contains(
                "could not put back ~/.agents/skills/shared-one: what the delete took is no \
                 longer in the trash"
            ),
            "{}",
            restored.describe_under(fx.home())
        );
    }

    #[test]
    fn restore_cannot_write_a_link_whose_target_was_never_recorded() {
        let fx = Fixture::realistic();
        let path = fx.agent("cursor").join("shared-one");

        let restored = fx.installer().restore(&[Change::RemovedSymlink {
            path: path.clone(),
            target: None,
        }]);

        assert!(restored.is_noop());
        assert_eq!(
            restored.missed,
            vec![Missed {
                path: path.clone(),
                reason: MissReason::TargetUnknown,
            }]
        );
        assert!(fs::symlink_metadata(&path).is_err());
    }

    #[test]
    fn restore_refuses_a_change_naming_a_path_outside_every_scope() {
        let fx = Fixture::realistic();
        let before = listing(fx.home());
        // Hand-made changes, not ones a delete produced: the guard is what
        // stops a caller turning a restore into a write anywhere it likes.
        let changes = vec![
            Change::MovedToTrash {
                from: fx.home().join("Documents/secret"),
                to: fx.roots().trash_dir().join("secret-1"),
            },
            Change::RemovedSymlink {
                path: fx.home().join("Documents/link"),
                target: Some(PathBuf::from("secret")),
            },
        ];

        let restored = fx.installer().restore(&changes);

        assert!(restored.is_noop());
        assert_eq!(restored.missed.len(), 2);
        assert!(
            restored
                .missed
                .iter()
                .all(|miss| miss.reason == MissReason::OutsideScope),
            "{:?}",
            restored.missed
        );
        assert_eq!(before, listing(fx.home()), "and nothing was written");
    }

    #[test]
    fn restore_of_nothing_changes_nothing() {
        let fx = Fixture::realistic();
        let before = listing(fx.home());

        let restored = fx.installer().restore(&[]);

        assert!(restored.is_noop());
        assert_eq!(restored.directories(), 0);
        assert_eq!(restored.links(), 0);
        assert!(restored.missed.is_empty());
        assert_eq!(restored.describe_under(fx.home()), "");
        assert_eq!(before, listing(fx.home()));
    }

    // -- a half-finished operation reports the damage --------------------------

    #[test]
    fn delete_reports_the_links_it_removed_before_it_stopped() {
        let fx = Fixture::realistic();
        let plan = DeletePlan {
            name: "shared-one".into(),
            origin: None,
            // The second path is a real directory, so the delete refuses it —
            // after the first link is already gone.
            links: vec![
                fx.agent("claude-code").join("shared-one"),
                fx.agent("claude-code").join("claude-only"),
            ],
            ..DeletePlan::default()
        };

        let error = fx.installer().delete(&plan).unwrap_err();

        let done = error.completed().expect("the removed link is reported");
        assert_eq!(
            done.changes,
            [Change::RemovedSymlink {
                path: fx.agent("claude-code").join("shared-one"),
                target: Some(PathBuf::from("../../.agents/skills/shared-one")),
            }]
        );
        assert!(matches!(
            error,
            InstallError::Partial { ref source, .. } if matches!(**source, InstallError::NotALink { .. })
        ));
        let message = error.describe_under(fx.home());
        assert!(
            message.starts_with("removed the link ~/.claude/skills/shared-one\nThen it stopped: "),
            "{message}"
        );
        assert!(message.contains("A real directory already sits at ~/.claude/skills/claude-only"));
    }

    #[test]
    fn a_delete_that_changed_nothing_stays_the_plain_refusal() {
        let fx = Fixture::realistic();
        let plan = DeletePlan {
            name: "claude-only".into(),
            origin: None,
            links: vec![fx.agent("claude-code").join("claude-only")],
            ..DeletePlan::default()
        };
        let error = fx.installer().delete(&plan).unwrap_err();
        assert!(matches!(error, InstallError::NotALink { .. }));
        assert!(error.completed().is_none(), "nothing to report");
    }

    #[test]
    fn consolidate_reports_the_copy_it_took_away_before_it_stopped() {
        let fx = Fixture::realistic();
        fan_out(&fx);
        // A file where a duplicate directory should be: the second entry is
        // refused, after the first has already been replaced.
        let blocked = fx.write_file(".cursor/skills/blocked", "not a directory\n");
        let origin = fx.shared().join("fanned-out");
        let plan = ConsolidatePlan {
            name: "fanned-out".into(),
            origin: origin.clone(),
            duplicates: vec![
                Duplicate {
                    path: fx.agent("claude-code").join("fanned-out"),
                    agent_id: "claude-code",
                    diff: ContentDiff::default(),
                    forced: false,
                },
                Duplicate {
                    path: blocked.clone(),
                    agent_id: "cursor",
                    diff: ContentDiff::default(),
                    forced: false,
                },
            ],
            skipped: Vec::new(),
        };

        let error = fx.installer().consolidate(&plan).unwrap_err();

        let done = error.completed().expect("the replaced copy is reported");
        assert!(
            matches!(done.changes[0], Change::MovedToTrash { .. }),
            "{:?}",
            done.changes
        );
        assert!(matches!(done.changes[1], Change::CreatedSymlink { .. }));
        let message = error.describe_under(fx.home());
        assert!(
            message.contains("moved ~/.claude/skills/fanned-out to the trash at"),
            "{message}"
        );
        assert!(message.contains("Then it stopped: "), "{message}");
        assert!(blocked.is_file(), "the file that was refused is untouched");
    }

    // -- messages that name a next step ---------------------------------------

    #[test]
    fn a_permission_failure_names_the_path_and_what_to_do_about_it() {
        let error = InstallError::io(
            Path::new("/Users/someone/.claude/skills/thing"),
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        );
        let message = error.describe_under(Path::new("/Users/someone"));
        assert_eq!(
            message,
            "Skillbase is not allowed to write to ~/.claude/skills/thing. Check that path in \
             Finder with File > Get Info, give yourself write access, then try again."
        );
        assert!(
            !message.contains("os error"),
            "the errno is not a next step"
        );
    }

    #[test]
    fn an_io_failure_that_is_not_a_permission_failure_still_names_the_error() {
        let error = InstallError::io(
            Path::new("/Users/someone/.claude/skills/thing"),
            std::io::Error::from(std::io::ErrorKind::NotFound),
        );
        assert!(
            error
                .describe_under(Path::new("/Users/someone"))
                .starts_with("~/.claude/skills/thing: ")
        );
    }

    #[test]
    fn a_refusal_writes_the_home_directory_as_a_tilde() {
        let home = Path::new("/Users/someone");
        let error = InstallError::AlreadyExists {
            path: home.join(".agents/skills/thing"),
        };
        assert_eq!(
            error.describe_under(home),
            "~/.agents/skills/thing is already taken. Rename or remove what is there, then try \
             again."
        );
        assert!(
            error.to_string().contains("/Users/someone/.agents"),
            "Display still writes the path in full, for a log"
        );
        assert_eq!(
            InstallError::AlreadyExists {
                path: PathBuf::from("/opt/skills/thing")
            }
            .describe_under(home),
            "/opt/skills/thing is already taken. Rename or remove what is there, then try again.",
            "a path outside home is left alone"
        );
    }

    #[test]
    fn a_failed_rollback_names_both_failures_and_where_the_skill_ended_up() {
        let home = Path::new("/Users/someone");
        let error = InstallError::Rollback {
            cause: Box::new(InstallError::NotALink {
                path: home.join(".claude/skills/thing"),
                kind: "directory",
            }),
            undo: Box::new(InstallError::io(
                home.join(".agents/skills/thing"),
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            )),
            at: home.join(".agents/skills/thing"),
            dangling_links: false,
        };

        let message = error.describe_under(home);
        let lines: Vec<&str> = message.lines().collect();
        assert_eq!(
            lines[0],
            "A real directory already sits at ~/.claude/skills/thing. Rename or remove it, then \
             try again."
        );
        assert!(
            lines[1]
                .starts_with("Putting things back failed too: Skillbase is not allowed to write"),
            "{}",
            lines[1]
        );
        assert_eq!(
            lines[2],
            "The skill's files are at ~/.agents/skills/thing. Check that path in Finder before \
             trying again."
        );
    }

    #[test]
    fn a_rollback_that_left_links_behind_says_so() {
        let home = Path::new("/Users/someone");
        let error = InstallError::Rollback {
            cause: Box::new(InstallError::NotALink {
                path: home.join(".claude/skills/thing"),
                kind: "directory",
            }),
            undo: Box::new(InstallError::io(
                home.join(".claude/skills/thing"),
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            )),
            at: home.join(".agents/skills/thing"),
            dangling_links: true,
        };

        let last = error
            .describe_under(home)
            .lines()
            .last()
            .unwrap()
            .to_string();
        assert_eq!(
            last,
            "The skill's files are at ~/.agents/skills/thing, but some links to it may still \
             point at the path it was moved to, which is no longer there. Check that path in \
             Finder before trying again.",
            "the files being back is not the whole state"
        );
    }

    #[test]
    fn a_rollback_reports_the_changes_its_cause_recorded() {
        let home = Path::new("/Users/someone");
        let moved = Change::Moved {
            from: home.join(".agents/skills/shared-one"),
            to: home.join(".agents/skills/shared-uno"),
        };
        let error = InstallError::Rollback {
            cause: Box::new(InstallError::partial(
                Outcome::one(moved.clone()),
                InstallError::NotALink {
                    path: home.join(".claude/skills/shared-one"),
                    kind: "directory",
                },
            )),
            undo: Box::new(InstallError::io(
                home.join(".agents/skills/shared-uno"),
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            )),
            at: home.join(".agents/skills/shared-uno"),
            dangling_links: false,
        };

        let done = error
            .completed()
            .expect("a rollback always changed the disk");
        assert_eq!(done.changes, [moved], "the rename that still stands");
        assert!(!done.is_noop());
    }

    #[test]
    fn a_rollback_whose_cause_recorded_nothing_still_reports_a_change() {
        let home = Path::new("/Users/someone");
        let error = InstallError::Rollback {
            cause: Box::new(InstallError::NotALink {
                path: home.join(".claude/skills/thing"),
                kind: "directory",
            }),
            undo: Box::new(InstallError::io(
                home.join(".agents/skills/thing"),
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            )),
            at: home.join(".agents/skills/thing"),
            dangling_links: false,
        };

        let done = error
            .completed()
            .expect("a rollback always changed the disk");
        assert_eq!(
            done.changes,
            [Change::NotUndone {
                path: home.join(".agents/skills/thing")
            }]
        );
        assert!(
            !done.is_noop(),
            "a caller asking whether to re-read the list has to be told yes"
        );
    }

    #[test]
    fn release_puts_the_link_back_when_the_move_fails_and_reports_the_move() {
        use std::os::unix::fs::PermissionsExt as _;
        let fx = Fixture::realistic();
        let installer = fx.installer();
        let dest = fx.agent("claude-code").join("shared-one");
        let store = fx.store();

        // A read-only store lets the link come off and then refuses the move,
        // which is the shape the rollback exists for.
        let before = fs::metadata(&store).unwrap().permissions();
        fs::set_permissions(&store, fs::Permissions::from_mode(0o500)).unwrap();
        if fs::write(store.join(".probe"), "").is_ok() {
            // Running as a user the permission bits do not stop, so there is
            // nothing here to fail.
            fs::remove_file(store.join(".probe")).ok();
            fs::set_permissions(&store, before).unwrap();
            return;
        }

        let error = installer.release("shared-one", &dest).unwrap_err();
        fs::set_permissions(&store, before).unwrap();

        assert!(matches!(error, InstallError::Io { .. }), "{error}");
        assert!(
            fs::symlink_metadata(&dest)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link went back, so the skill is still reachable"
        );
        assert_eq!(
            fs::canonicalize(&dest).unwrap(),
            fx.store().join("shared-one")
        );
    }

    // -- consolidate ----------------------------------------------------------

    const FANNED: &str =
        "---\nname: fanned-out\ndescription: Copied everywhere.\n---\n\n# fanned-out\n";

    /// One skill fanned out as four independent real directories: the origin in
    /// the shared directory, two exact copies, and one edited after it was
    /// copied. This is the shape consolidation exists for, and the shape the
    /// machine this was written on actually has.
    fn fan_out(fx: &Fixture) {
        for dir in [
            ".agents/skills/fanned-out",
            ".claude/skills/fanned-out",
            ".cursor/skills/fanned-out",
            ".gemini/skills/fanned-out",
        ] {
            fx.write_file(&format!("{dir}/SKILL.md"), FANNED);
            fx.write_file(&format!("{dir}/scripts/run.sh"), "echo one\n");
        }
        // Gemini's copy has been edited since. Those bytes are the user's, and
        // no default action may throw them away.
        fx.write_file(
            ".gemini/skills/fanned-out/SKILL.md",
            &FANNED.replace("Copied everywhere.", "Edited here, and only here."),
        );
    }

    fn plan_for(fx: &Fixture, name: &str) -> ConsolidatePlan {
        let result = fx.scan();
        fx.installer()
            .plan_consolidate(result.get(name).expect("the skill was discovered"))
    }

    #[test]
    fn plan_consolidate_separates_exact_copies_from_edited_ones() {
        let fx = Fixture::realistic();
        fan_out(&fx);
        let plan = plan_for(&fx, "fanned-out");

        assert_eq!(plan.origin(), fx.shared().join("fanned-out"));
        assert_eq!(plan.duplicates().len(), 3);
        assert_eq!(plan.identical_count(), 2);
        assert_eq!(plan.differing_count(), 1);
        assert_eq!(plan.replace_count(), 2, "only the exact copies by default");
        assert_eq!(plan.skipped_count(), 1);
        assert!(plan.skipped().is_empty());

        let paths: Vec<_> = plan.duplicates().iter().map(Duplicate::path).collect();
        assert_eq!(
            paths,
            [
                fx.agent("claude-code").join("fanned-out"),
                fx.agent("cursor").join("fanned-out"),
                fx.agent("gemini-cli").join("fanned-out"),
            ],
            "registry order, and each labelled with its own agent"
        );
        assert_eq!(
            plan.duplicates()
                .iter()
                .map(Duplicate::agent_id)
                .collect::<Vec<_>>(),
            ["claude-code", "cursor", "gemini-cli"]
        );

        let edited = plan.differing().next().unwrap();
        assert_eq!(edited.path(), fx.agent("gemini-cli").join("fanned-out"));
        assert_eq!(edited.diff().differing, [PathBuf::from("SKILL.md")]);
        assert_eq!(edited.diff().summary(), "1 file differs");
        assert!(!edited.forced(), "nothing is forced until it is asked for");
        assert!(!edited.will_be_replaced());
    }

    #[test]
    fn consolidate_replaces_exact_copies_with_relative_symlinks() {
        let fx = Fixture::realistic();
        fan_out(&fx);
        let plan = plan_for(&fx, "fanned-out");
        let outcome = fx.installer().consolidate(&plan).unwrap();

        for agent in ["claude-code", "cursor"] {
            let link = fx.agent(agent).join("fanned-out");
            let meta = fs::symlink_metadata(&link).unwrap();
            assert!(meta.file_type().is_symlink(), "{agent} is now a link");
            assert_eq!(
                fs::read_link(&link).unwrap(),
                Path::new("../../.agents/skills/fanned-out"),
                "and a relative one"
            );
            assert_eq!(
                fs::read_to_string(link.join(SKILL_FILE_NAME)).unwrap(),
                FANNED,
                "the link resolves to the origin's bytes"
            );
        }
        assert_eq!(
            outcome
                .changes
                .iter()
                .filter(|c| matches!(c, Change::CreatedSymlink { .. }))
                .count(),
            2
        );
        assert_eq!(
            outcome
                .changes
                .iter()
                .filter(|c| matches!(c, Change::MovedToTrash { .. }))
                .count(),
            2,
            "the copies went to the trash, not to nowhere"
        );
    }

    #[test]
    fn consolidate_refuses_the_edited_copy_and_leaves_it_exactly_as_it_was() {
        let fx = Fixture::realistic();
        fan_out(&fx);
        let edited = fx.agent("gemini-cli").join("fanned-out");
        let before = listing(&edited);

        let outcome = fx
            .installer()
            .consolidate(&plan_for(&fx, "fanned-out"))
            .unwrap();

        assert!(
            !fs::symlink_metadata(&edited)
                .unwrap()
                .file_type()
                .is_symlink(),
            "still a real directory"
        );
        assert_eq!(before, listing(&edited), "byte for byte as it was");
        assert!(
            fs::read_to_string(edited.join(SKILL_FILE_NAME))
                .unwrap()
                .contains("Edited here, and only here."),
            "the divergent edit survives"
        );
        assert!(
            outcome.changes.contains(&Change::NoChange {
                path: edited,
                reason: "different from the origin, so it was left alone",
            }),
            "and the refusal is reported, not swallowed: {}",
            outcome.describe()
        );
    }

    #[test]
    fn a_difference_in_a_bundled_file_is_detected() {
        let fx = Fixture::realistic();
        fan_out(&fx);
        // Same SKILL.md, different script: still a divergence.
        fx.write_file(".claude/skills/fanned-out/scripts/run.sh", "echo two\n");
        // An extra file on one side, and a missing one on the other.
        fx.write_file(".cursor/skills/fanned-out/references/notes.md", "extra\n");
        fs::remove_file(fx.agent("gemini-cli").join("fanned-out/scripts/run.sh")).unwrap();

        let plan = plan_for(&fx, "fanned-out");
        assert_eq!(plan.identical_count(), 0);
        assert_eq!(plan.replace_count(), 0);

        let by_agent = |id: &str| {
            plan.duplicates()
                .iter()
                .find(|d| d.agent_id() == id)
                .unwrap()
                .diff()
                .clone()
        };
        assert_eq!(
            by_agent("claude-code").differing,
            [PathBuf::from("scripts/run.sh")]
        );
        assert_eq!(
            by_agent("cursor").extra,
            [
                PathBuf::from("references"),
                PathBuf::from("references/notes.md")
            ]
        );
        assert_eq!(by_agent("cursor").summary(), "2 extra files");
        assert_eq!(
            by_agent("gemini-cli").missing,
            [PathBuf::from("scripts/run.sh")]
        );

        let before = listing(fx.home());
        assert!(fx.installer().consolidate(&plan).unwrap().is_noop());
        assert_eq!(before, listing(fx.home()), "nothing was touched");
    }

    #[test]
    fn an_identical_copy_that_carries_the_copy_marker_still_counts_as_identical() {
        let fx = Fixture::realistic();
        fan_out(&fx);
        // Skillbase's own bookkeeping is not part of the skill.
        fx.write_file(".claude/skills/fanned-out/.skillbase-copy", "/somewhere\n");
        let plan = plan_for(&fx, "fanned-out");
        assert_eq!(plan.identical_count(), 2);
    }

    #[test]
    fn forcing_one_duplicate_replaces_that_one_and_no_other() {
        let fx = Fixture::realistic();
        fan_out(&fx);
        // Edit a second copy so there are two divergences and only one is forced.
        fx.write_file(".cursor/skills/fanned-out/scripts/run.sh", "echo three\n");

        let mut plan = plan_for(&fx, "fanned-out");
        assert_eq!(plan.differing_count(), 2);
        assert!(
            !plan.force(Path::new("/nowhere")),
            "an unknown path forces nothing"
        );
        assert_eq!(plan.forced_count(), 0);

        let gemini = fx.agent("gemini-cli").join("fanned-out");
        assert!(plan.force(&gemini));
        assert!(plan.is_forced(&gemini));
        assert_eq!(plan.forced_count(), 1);
        assert_eq!(
            plan.replace_count(),
            2,
            "the exact copy plus the forced one"
        );
        assert_eq!(plan.skipped_count(), 1);

        fx.installer().consolidate(&plan).unwrap();
        assert!(
            fs::symlink_metadata(&gemini)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the forced copy was replaced"
        );
        assert!(
            fx.agent("cursor")
                .join("fanned-out/scripts/run.sh")
                .is_file(),
            "the copy that was not forced kept its own bytes"
        );
        assert!(
            !fs::symlink_metadata(fx.agent("cursor").join("fanned-out"))
                .unwrap()
                .file_type()
                .is_symlink()
        );

        plan.unforce(&gemini);
        assert!(!plan.is_forced(&gemini));
    }

    #[test]
    fn consolidating_a_duplicate_that_is_already_a_link_does_nothing() {
        let fx = Fixture::realistic();
        fan_out(&fx);
        let installer = fx.installer();
        let plan = plan_for(&fx, "fanned-out");
        installer.consolidate(&plan).unwrap();

        let after_first = listing(fx.home());
        let again = installer.consolidate(&plan).unwrap();
        assert!(again.is_noop(), "{}", again.describe());
        assert!(again.changes.contains(&Change::NoChange {
            path: fx.agent("cursor").join("fanned-out"),
            reason: "a link, not a copy",
        }));
        assert_eq!(after_first, listing(fx.home()));
    }

    #[test]
    fn consolidate_never_removes_the_origin_even_when_it_is_listed() {
        let fx = Fixture::realistic();
        fan_out(&fx);
        let origin = fx.shared().join("fanned-out");
        let plan = ConsolidatePlan {
            name: "fanned-out".into(),
            origin: origin.clone(),
            duplicates: vec![Duplicate {
                path: origin.clone(),
                agent_id: "shared",
                diff: ContentDiff::default(),
                forced: true,
            }],
            skipped: Vec::new(),
        };
        let outcome = fx.installer().consolidate(&plan).unwrap();
        assert!(outcome.is_noop());
        assert!(origin.join(SKILL_FILE_NAME).is_file());
    }

    #[test]
    fn consolidate_refuses_every_path_outside_the_managed_scopes() {
        let fx = Fixture::realistic();
        fan_out(&fx);
        let before = listing(fx.home());
        let outside = fx.home().join("Documents/secret");

        // A duplicate outside every scope.
        let plan = ConsolidatePlan {
            name: "fanned-out".into(),
            origin: fx.shared().join("fanned-out"),
            duplicates: vec![Duplicate {
                path: outside.clone(),
                agent_id: "claude-code",
                diff: ContentDiff::default(),
                forced: true,
            }],
            skipped: Vec::new(),
        };
        assert!(matches!(
            fx.installer().consolidate(&plan),
            Err(InstallError::OutsideScope { .. })
        ));

        // An origin outside every scope, which would otherwise become the
        // target of every link written.
        let plan = ConsolidatePlan {
            name: "fanned-out".into(),
            origin: outside.clone(),
            duplicates: vec![Duplicate {
                path: fx.agent("cursor").join("fanned-out"),
                agent_id: "cursor",
                diff: ContentDiff::default(),
                forced: true,
            }],
            skipped: Vec::new(),
        };
        assert!(matches!(
            fx.installer().consolidate(&plan),
            Err(InstallError::OutsideScope { .. })
        ));

        assert_eq!(before, listing(fx.home()), "a refusal changes nothing");
        assert!(outside.join("keep-me.txt").is_file());
    }

    #[test]
    fn plan_consolidate_skips_a_location_outside_every_scope() {
        let fx = Fixture::realistic();
        fan_out(&fx);
        let outside = fx.home().join("Documents/secret");
        let skill = DiscoveredSkill {
            name: "fanned-out".into(),
            origin: fx.shared().join("fanned-out"),
            locations: vec![
                Location {
                    agent_id: "cursor",
                    path: fx.agent("cursor").join("fanned-out"),
                    kind: LocationKind::Copy,
                },
                Location {
                    agent_id: "cursor",
                    path: outside.clone(),
                    kind: LocationKind::Copy,
                },
            ],
            doc: None,
            parse_error: None,
            conflicts: vec![outside.clone()],
            managed: false,
            codex_disabled: false,
        };

        let plan = fx.installer().plan_consolidate(&skill);
        assert_eq!(
            plan.skipped(),
            std::slice::from_ref(&outside),
            "named once, not twice"
        );
        assert_eq!(plan.duplicates().len(), 1);
        assert_eq!(
            plan.duplicates()[0].path(),
            fx.agent("cursor").join("fanned-out")
        );
        assert!(outside.join("keep-me.txt").is_file());
    }

    #[test]
    fn consolidate_then_re_discover_shows_one_origin_and_links_to_it() {
        let fx = Fixture::realistic();
        fan_out(&fx);
        let installer = fx.installer();

        let before = fx.scan();
        let skill = before.get("fanned-out").unwrap();
        assert_eq!(skill.conflicts.len(), 3, "three real directories drifting");

        installer
            .consolidate(&installer.plan_consolidate(skill))
            .unwrap();

        let after = fx.scan();
        let skill = after.get("fanned-out").unwrap();
        assert_eq!(skill.origin, fx.shared().join("fanned-out"));
        assert_eq!(
            skill.conflicts,
            [fx.agent("gemini-cli").join("fanned-out")],
            "only the copy that was refused is still a conflict"
        );
        let kinds: Vec<_> = skill
            .locations
            .iter()
            .map(|l| (l.agent_id, l.kind.clone()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("shared", LocationKind::Origin),
                (
                    "claude-code",
                    LocationKind::Symlink {
                        target: fx.shared().join("fanned-out")
                    }
                ),
                (
                    "cursor",
                    LocationKind::Symlink {
                        target: fx.shared().join("fanned-out")
                    }
                ),
                ("gemini-cli", LocationKind::Copy),
            ]
        );
        assert!(skill.visible_to().contains(&"claude-code"));
    }

    #[test]
    fn describe_under_writes_home_as_a_tilde_and_counts_the_rest() {
        let home = Path::new("/Users/someone");
        let mut outcome = Outcome::default();
        for n in 0..9 {
            outcome.push(Change::RemovedSymlink {
                path: home.join(format!(".claude/skills/skill-{n}")),
                target: None,
            });
        }

        let described = outcome.describe_under(home);
        let lines: Vec<&str> = described.lines().collect();

        assert_eq!(lines.len(), Outcome::MAX_DESCRIBED + 1);
        assert_eq!(lines[0], "removed the link ~/.claude/skills/skill-0");
        assert_eq!(lines[Outcome::MAX_DESCRIBED], "and 3 more changes");
        assert!(!described.contains("/Users/someone"));
    }

    #[test]
    fn describe_under_leaves_a_path_outside_home_alone() {
        let outcome = Outcome::one(Change::RemovedSymlink {
            path: PathBuf::from("/opt/skills/thing"),
            target: None,
        });

        assert_eq!(
            outcome.describe_under(Path::new("/Users/someone")),
            "removed the link /opt/skills/thing"
        );
    }

    #[test]
    fn consolidate_leaves_a_skill_with_no_duplicates_alone() {
        let fx = Fixture::realistic();
        let before = listing(fx.home());
        let result = fx.scan();
        let plan = fx
            .installer()
            .plan_consolidate(result.get("shared-one").unwrap());
        assert!(plan.is_empty());
        assert!(
            fx.installer()
                .consolidate(&plan)
                .unwrap()
                .changes
                .is_empty()
        );
        assert_eq!(before, listing(fx.home()));
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
            created.visible_to().contains(&"cursor"),
            "the store is the directory the agents read, so a new skill is already there"
        );
        assert!(
            !created.visible_to().contains(&"claude-code"),
            "Claude Code does not read it and still needs a link"
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
        let shared = installer
            .link("new-thing", &dir, Registry::shared())
            .unwrap();
        assert!(
            shared.is_noop(),
            "the skill was written into the shared directory to begin with"
        );

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
    // -- import ---------------------------------------------------------------

    #[test]
    fn import_copies_a_directory_from_outside_into_the_store() {
        let fx = Fixture::realistic();
        let source = fx.skill("Downloads/my-skill", "my-skill");
        fx.write_file("Downloads/my-skill/references/notes.md", "notes\n");

        let (dir, outcome) = fx
            .installer()
            .import(&source, &ImportOptions::new())
            .unwrap();

        assert_eq!(dir, fx.store().join("my-skill"));
        assert_eq!(
            outcome.changes,
            vec![Change::Copied {
                from: source.clone(),
                to: dir.clone(),
            }],
            "the one change says copied, not moved, because that is what happened"
        );
        assert!(dir.join("references/notes.md").is_file());
        assert!(
            source.join(SKILL_FILE_NAME).is_file(),
            "an import leaves the user's own directory exactly where it was"
        );

        let found = fx.scan();
        let imported = found.get("my-skill").unwrap();
        assert!(imported.managed);
        assert_eq!(imported.origin, dir);
    }

    #[test]
    fn import_names_the_skill_from_its_frontmatter() {
        let fx = Fixture::realistic();
        let source = fx.skill("Downloads/downloaded-42", "pdf-tools");
        let (dir, _) = fx
            .installer()
            .import(&source, &ImportOptions::new())
            .unwrap();
        assert_eq!(
            dir,
            fx.store().join("pdf-tools"),
            "the frontmatter name is the name the agents use, so it is the directory's too"
        );
    }

    #[test]
    fn import_falls_back_when_the_frontmatter_has_no_usable_name() {
        let fx = Fixture::realistic();
        let installer = fx.installer();

        let loud = fx.skill("Downloads/loud", "My Skill");
        let (dir, _) = installer.import(&loud, &ImportOptions::new()).unwrap();
        assert_eq!(dir, fx.store().join("my-skill"), "a name is slugified");

        fx.write_file("Downloads/Some Folder/SKILL.md", "# No frontmatter\n");
        let unparsed = fx.home().join("Downloads/Some Folder");
        let (dir, _) = installer.import(&unparsed, &ImportOptions::new()).unwrap();
        assert_eq!(
            dir,
            fx.store().join("some-folder"),
            "with nothing in the frontmatter the directory's own name stands in"
        );
    }

    #[test]
    fn import_refuses_a_directory_that_holds_no_skill_and_says_what_to_look_for() {
        let fx = Fixture::realistic();
        let repo = fx.dir("Downloads/some-repo");
        fx.skill("Downloads/some-repo/skills/the-real-one", "the-real-one");
        let before = listing(fx.home());

        let error = fx
            .installer()
            .import(&repo, &ImportOptions::new())
            .unwrap_err();

        assert!(matches!(error, InstallError::NotASkill { .. }));
        let message = error.describe_under(fx.home());
        assert!(message.contains("~/Downloads/some-repo"), "{message}");
        assert!(message.contains("SKILL.md"), "{message}");
        assert!(
            message.contains("skills"),
            "picking the repository is the common mistake, so the message names the way \
             out of it: {message}"
        );
        assert_eq!(before, listing(fx.home()), "nothing was written");
    }

    #[test]
    fn import_refuses_a_file_and_points_at_the_folder_around_it() {
        let fx = Fixture::realistic();
        let source = fx.skill("Downloads/my-skill", "my-skill");
        let error = fx
            .installer()
            .import(&source.join(SKILL_FILE_NAME), &ImportOptions::new())
            .unwrap_err();

        assert!(matches!(error, InstallError::NotASkill { .. }));
        assert!(
            error.describe_under(fx.home()).contains("folder"),
            "{error}"
        );
    }

    #[test]
    fn import_refuses_a_path_that_is_not_there() {
        let fx = Fixture::realistic();
        let error = fx
            .installer()
            .import(&fx.home().join("Downloads/gone"), &ImportOptions::new())
            .unwrap_err();

        assert!(matches!(error, InstallError::NotASkill { .. }));
        assert!(
            error.describe_under(fx.home()).contains("nothing at"),
            "{error}"
        );
    }

    #[test]
    fn import_refuses_a_source_that_is_already_managed_and_points_at_adopt() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        let before = listing(fx.home());

        for path in [
            fx.store().join("shared-one"),
            fx.private().join("hidden-one"),
            fx.agent("claude-code").join("claude-only"),
            fx.store(),
            fx.agent("claude-code"),
            // A directory that holds the store rather than sitting in it:
            // copying it in would copy the store into itself.
            fx.home().to_path_buf(),
        ] {
            let error = installer.import(&path, &ImportOptions::new()).unwrap_err();
            assert!(
                matches!(error, InstallError::AlreadyManaged { .. }),
                "{} should have been refused, got {error}",
                path.display()
            );
            assert!(
                error.describe_under(fx.home()).contains("Adopt"),
                "the message has to name the operation that does apply: {error}"
            );
        }
        assert_eq!(before, listing(fx.home()));
    }

    #[test]
    fn import_refuses_a_symlink_that_leads_back_into_a_managed_directory() {
        let fx = Fixture::realistic();
        fx.dir("Downloads");
        let source = fx.home().join("Downloads/looks-outside");
        symlink(fx.store().join("shared-one"), &source).unwrap();

        let error = fx
            .installer()
            .import(&source, &ImportOptions::new())
            .unwrap_err();
        assert!(
            matches!(error, InstallError::AlreadyManaged { .. }),
            "a link is the skill it points at, and importing through one would make a \
             second copy of it: {error}"
        );
    }

    #[test]
    fn import_refuses_a_name_that_is_taken_and_keeps_both_when_asked() {
        let fx = Fixture::realistic();
        let installer = fx.installer();
        let source = fx.dir("Downloads/shared-two");
        fs::write(
            source.join(SKILL_FILE_NAME),
            "---\nname: shared-two\ndescription: The imported one.\n---\n",
        )
        .unwrap();

        let error = installer
            .import(&source, &ImportOptions::new())
            .unwrap_err();
        assert!(
            matches!(&error, InstallError::AlreadyExists { path } if *path == fx.store().join("shared-two")),
            "{error}"
        );
        assert!(
            !fs::read_to_string(fx.store().join("shared-two/SKILL.md"))
                .unwrap()
                .contains("imported"),
            "the skill that was there is untouched"
        );

        let (dir, _) = installer
            .import(&source, &ImportOptions::new().named("shared-two-2"))
            .unwrap();
        assert_eq!(dir, fx.store().join("shared-two-2"));
        assert!(
            fs::read_to_string(dir.join(SKILL_FILE_NAME))
                .unwrap()
                .contains("imported")
        );
    }

    #[test]
    fn import_replaces_only_when_told_to_and_the_old_skill_is_recoverable() {
        let fx = Fixture::realistic();
        let source = fx.dir("Downloads/shared-two");
        fs::write(
            source.join(SKILL_FILE_NAME),
            "---\nname: shared-two\ndescription: The imported one.\n---\n",
        )
        .unwrap();

        let (dir, outcome) = fx
            .installer()
            .import(&source, &ImportOptions::new().replacing())
            .unwrap();

        assert_eq!(dir, fx.store().join("shared-two"));
        assert!(
            fs::read_to_string(dir.join(SKILL_FILE_NAME))
                .unwrap()
                .contains("imported")
        );
        let trashed = match outcome.changes.first() {
            Some(Change::MovedToTrash { to, .. }) => to.clone(),
            other => panic!("the skill that was replaced has to be recoverable: {other:?}"),
        };
        assert!(
            fs::read_to_string(trashed.join(SKILL_FILE_NAME))
                .unwrap()
                .contains("The shared-two skill"),
            "the old directory is in the trash, not gone"
        );
    }

    #[test]
    fn import_refuses_a_name_override_that_is_not_a_usable_name() {
        let fx = Fixture::realistic();
        let source = fx.skill("Downloads/my-skill", "my-skill");
        let before = listing(fx.home());

        for name in ["Not Kebab", "under_score", "", "../escape"] {
            let error = fx
                .installer()
                .import(&source, &ImportOptions::new().named(name))
                .unwrap_err();
            assert!(
                matches!(error, InstallError::InvalidName { .. }),
                "`{name}` should have been refused, got {error}"
            );
        }
        assert_eq!(before, listing(fx.home()));
    }

    #[test]
    fn import_leaves_the_checkouts_git_directory_behind() {
        let fx = Fixture::realistic();
        let source = fx.skill("Downloads/my-skill", "my-skill");
        fx.write_file("Downloads/my-skill/.git/config", "[core]\n");
        fx.write_file("Downloads/my-skill/.gitignore", "target\n");

        let (dir, _) = fx
            .installer()
            .import(&source, &ImportOptions::new())
            .unwrap();

        assert!(
            fs::symlink_metadata(dir.join(".git")).is_err(),
            "the history is the checkout's, not the skill's"
        );
        assert!(
            dir.join(".gitignore").is_file(),
            "only the history is left behind, not everything with a dot"
        );
    }

    #[test]
    fn import_does_not_follow_a_symlink_out_of_the_source_tree() {
        let fx = Fixture::realistic();
        let source = fx.skill("Downloads/my-skill", "my-skill");
        fx.write_file("Downloads/my-skill/references/inside.md", "inside\n");
        fx.dir("Downloads/my-skill/scripts");
        symlink("../references/inside.md", source.join("scripts/near.md")).unwrap();
        symlink(
            source.join("references/inside.md"),
            source.join("also-inside.md"),
        )
        .unwrap();
        symlink(fx.home().join("Documents/secret"), source.join("secret")).unwrap();
        symlink("../../../Documents/secret", source.join("climbing-out")).unwrap();

        let (dir, _) = fx
            .installer()
            .import(&source, &ImportOptions::new())
            .unwrap();

        assert!(
            fs::symlink_metadata(dir.join("scripts/near.md"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "a link that stays inside the tree is recreated as it was written"
        );
        assert!(fs::symlink_metadata(dir.join("also-inside.md")).is_ok());
        for escaping in ["secret", "climbing-out"] {
            assert!(
                fs::symlink_metadata(dir.join(escaping)).is_err(),
                "`{escaping}` points out of the tree, so it is left behind rather than \
                 followed"
            );
        }
        assert!(
            !dir.join("secret/keep-me.txt").exists(),
            "nothing outside the picked directory was copied"
        );
    }
}
