//! Installing skills from GitHub, and telling when one has moved on.
//!
//! # Two questions, two mechanisms
//!
//! **Has upstream changed?** Compare the `github-tree-sha` recorded in the
//! installed `SKILL.md` against the tree sha GitHub reports for the same path
//! now. Sha to sha, nothing hashed locally, and cheap enough to ask for every
//! installed skill at once — see [`check_updates`].
//!
//! **Has the user edited it?** Compare a SHA-256 [`content_digest`] of the
//! installed directory against the digest recorded at install time in
//! [`REMOTE_CACHE_FILE`]. This is deliberately *not* git's tree-hash format.
//! Skillbase writes provenance into the installed `SKILL.md`, so the installed
//! directory can never hash to the upstream tree sha whatever format is used;
//! a plain deterministic digest answers the question and is far simpler.
//!
//! # The cache is disposable, but not silently
//!
//! [`REMOTE_CACHE_FILE`] is modelled on the usage cache: a version, and
//! missing, corrupt or differently versioned all mean "work it out again"
//! rather than an error. Losing it costs speed, never correctness:
//! [`upstream_state`] answers the same question by downloading upstream and
//! comparing, and update checking still works because the baseline sha lives in
//! the `SKILL.md`, not in the cache.
//!
//! A cache that cannot be *written* is different. It is not one lost answer, it
//! is every answer from now on: every skill reports [`LocalState::Unknown`] for
//! ever, and every check spends the hourly GitHub budget again. So
//! [`RemoteCache::write`] hands back a [`CacheWriteError`] instead of
//! swallowing it. It is still not fatal — nothing here stops the install that
//! just succeeded from having succeeded — it is only no longer invisible.
//!
//! Every call here blocks. Run them on a background task.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::discovery::DiscoveredSkill;
use crate::doc::SkillDoc;
use crate::error::SkillError;
use crate::github::{GitHub, GitHubError, RateLimit, RepoRef, SkillLocation};
use crate::http::Http;
use crate::install::{
    Change, DeletePlan, InstallError, Installer, Outcome, copy_dir, crosses_filesystems,
};
use crate::provenance::{Provenance, SkillLock};
use crate::registry::Roots;
use crate::skill::SKILL_FILE_NAME;
use crate::slug::{is_kebab_case, slugify};

/// Skillbase's remote cache, relative to the home directory.
pub const REMOTE_CACHE_FILE: &str = ".skillbase/remote.json";

/// Where downloads are unpacked before they are moved into the store,
/// relative to the home directory.
///
/// A skill is assembled here — extracted, checked for a `SKILL.md`, given its
/// provenance — and only then renamed into place, so a failed download never
/// leaves half a skill in `~/.agents/skills`.
pub const STAGING_DIR: &str = ".skillbase/staging";

/// Bumped whenever a record in the cache changes shape. An older or newer
/// cache is discarded rather than migrated.
const CACHE_VERSION: u32 = 1;

/// Domain separator, so a digest of this crate's cannot collide with a digest
/// of the same bytes computed for something else.
const DIGEST_DOMAIN: &[u8] = b"skillbase-content-digest-v1";

// ---------------------------------------------------------------------------
// Content digest
// ---------------------------------------------------------------------------

/// A SHA-256 digest of everything in a directory, as lowercase hex.
///
/// Deterministic and order-independent: every path under `dir` is collected,
/// sorted, and fed to the hash with its kind, its length and its bytes. A file
/// contributes its content, a symlink contributes its target as written, and a
/// directory contributes its path alone. Nothing is followed, so a link into
/// the store hashes as a link rather than as whatever it points at.
///
/// Lengths are hashed alongside the bytes, so no rearrangement of names and
/// contents produces the same digest as a different tree.
pub fn content_digest(dir: &Path) -> Result<String, io::Error> {
    let mut entries = Vec::new();
    collect(dir, dir, &mut entries)?;
    entries.sort();

    let mut hasher = Sha256::new();
    hasher.update(DIGEST_DOMAIN);
    for (relative, kind) in &entries {
        let payload: Vec<u8> = match kind {
            Kind::Dir => Vec::new(),
            Kind::File => fs::read(dir.join(relative))?,
            Kind::Symlink => fs::read_link(dir.join(relative))?
                .as_os_str()
                .as_encoded_bytes()
                .to_vec(),
        };
        hasher.update([kind.tag()]);
        hasher.update((relative.len() as u64).to_le_bytes());
        hasher.update(relative.as_bytes());
        hasher.update((payload.len() as u64).to_le_bytes());
        hasher.update(&payload);
    }
    Ok(hex(&hasher.finalize()))
}

/// What one path in a skill directory is, for the digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Dir,
    File,
    Symlink,
}

impl Kind {
    fn tag(self) -> u8 {
        match self {
            Self::Dir => b'd',
            Self::File => b'f',
            Self::Symlink => b'l',
        }
    }
}

/// Records every path under `dir` by its path relative to `root`, using `/` as
/// the separator so a digest does not depend on the platform's spelling.
fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, Kind)>) -> Result<(), io::Error> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            out.push((relative, Kind::Symlink));
        } else if meta.is_dir() {
            out.push((relative, Kind::Dir));
            collect(root, &path, out)?;
        } else {
            out.push((relative, Kind::File));
        }
    }
    Ok(())
}

/// Lowercase hex.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

// ---------------------------------------------------------------------------
// The cache
// ---------------------------------------------------------------------------

/// What was recorded for one installed skill.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRecord {
    /// [`content_digest`] of the installed directory when it was written.
    #[serde(default)]
    pub digest: String,
    /// The tree sha it was installed from.
    #[serde(default)]
    pub tree_sha: String,
    /// The commit sha the ref pointed at when it was installed.
    ///
    /// The tree sha above answers "has upstream changed?" and is the cheaper
    /// question, but it is not a commit-ish: `github.com/o/r/compare/a...b`
    /// only resolves branches, tags and commits, and answers 404 for a tree.
    /// So the commit is recorded alongside it, and it is the one thing that
    /// lets the interface link at the difference itself.
    ///
    /// Empty for a skill installed before this was recorded, and for one
    /// another tool installed. Neither is an error; there is simply no
    /// comparison to offer, and the interface says so rather than linking at a
    /// page that does not exist.
    #[serde(default)]
    pub commit_sha: String,
    /// When it was recorded, in seconds since the Unix epoch.
    #[serde(default)]
    pub recorded_at: i64,
}

/// What the last check saw in one repository at one ref.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRecord {
    /// The commit sha the ref pointed at.
    #[serde(default)]
    pub commit_sha: String,
    /// Subdirectory path to its tree sha, with `""` for the repository root.
    /// A path that named no directory is kept as `None`, so it is not walked
    /// for again while the commit is unchanged.
    #[serde(default)]
    pub trees: BTreeMap<String, Option<String>>,
    /// The ETag of the ref response, sent as `If-None-Match` on the next check.
    /// Only used when a token is in use; see [`GitHub::ref_state`].
    #[serde(default)]
    pub etag: String,
    /// When it was checked, in seconds since the Unix epoch.
    #[serde(default)]
    pub checked_at: i64,
}

/// The on-disk cache at [`REMOTE_CACHE_FILE`].
///
/// Two tables. `skills` answers "was this edited locally?"; `repos` is what
/// makes an update check one request per repository rather than a tree walk per
/// skill.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteCache {
    /// [`CACHE_VERSION`]. A mismatch discards the whole cache.
    #[serde(default)]
    version: u32,
    /// Keyed by skill name.
    #[serde(default)]
    skills: BTreeMap<String, SkillRecord>,
    /// Keyed by [`repo_key`].
    #[serde(default)]
    repos: BTreeMap<String, RepoRecord>,
}

impl RemoteCache {
    /// An empty cache, which behaves exactly as a lost one.
    pub fn new() -> Self {
        Self {
            version: CACHE_VERSION,
            ..Self::default()
        }
    }

    /// The cache file's path under `roots`.
    pub fn path(roots: &Roots) -> PathBuf {
        roots.home().join(REMOTE_CACHE_FILE)
    }

    /// Reads the cache under `roots`.
    pub fn read(roots: &Roots) -> RemoteCache {
        Self::read_file(&Self::path(roots))
    }

    /// Reads a cache from an explicit path.
    ///
    /// Missing, unreadable, corrupt and written by another version all come
    /// back as an empty cache. None of them is worth reporting, because none of
    /// them changes an answer: they only cost the work of computing it again.
    pub fn read_file(path: &Path) -> RemoteCache {
        let Ok(text) = fs::read_to_string(path) else {
            return RemoteCache::new();
        };
        match serde_json::from_str::<RemoteCache>(&text) {
            Ok(cache) if cache.version == CACHE_VERSION => cache,
            _ => RemoteCache::new(),
        }
    }

    /// Writes the cache under `roots`, and returns what went wrong when
    /// anything did.
    ///
    /// Not fatal: nothing here stops an install or an update check from having
    /// worked. But a cache that never lands is not a slower Skillbase, it is a
    /// different one — every skill reports "no record of what was installed
    /// here" for ever, and every update check spends the hourly GitHub budget
    /// again — so the caller is handed the failure to say so. See
    /// [`CacheWriteError`].
    ///
    /// The return type is [`Option`] rather than [`Result`] so that a caller
    /// with nothing useful to do about it can go on writing `cache.write(&roots);`.
    pub fn write(&self, roots: &Roots) -> Option<CacheWriteError> {
        self.write_file(&Self::path(roots))
    }

    /// Writes the cache to an explicit path. As [`RemoteCache::write`].
    pub fn write_file(&self, path: &Path) -> Option<CacheWriteError> {
        self.try_write_file(path).err()
    }

    /// Written to a sibling and renamed, so an interrupted write leaves the
    /// previous cache rather than a truncated one.
    fn try_write_file(&self, path: &Path) -> Result<(), CacheWriteError> {
        let mut cache = self.clone();
        cache.version = CACHE_VERSION;
        let text = serde_json::to_string(&cache).map_err(CacheWriteError::Serialize)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| CacheWriteError::io(parent, e))?;
        }
        let temp = path.with_extension("json.tmp");
        fs::write(&temp, text).map_err(|e| CacheWriteError::io(&temp, e))?;
        fs::rename(&temp, path).map_err(|e| {
            let _ = fs::remove_file(&temp);
            CacheWriteError::io(path, e)
        })
    }

    /// What was recorded for one skill.
    pub fn skill(&self, name: &str) -> Option<&SkillRecord> {
        self.skills.get(name)
    }

    /// Records what a skill looked like when it was written.
    pub fn record_skill(&mut self, name: impl Into<String>, record: SkillRecord) {
        self.skills.insert(name.into(), record);
    }

    /// Forgets a skill, which is what an uninstall should do.
    pub fn forget_skill(&mut self, name: &str) -> Option<SkillRecord> {
        self.skills.remove(name)
    }

    /// Forgets the record of every skill a delete removed, and returns true
    /// when the cache changed and has to be written.
    ///
    /// A record left behind after a delete answers for whatever takes the name
    /// next, because the records are keyed by name: the next skill created,
    /// imported or renamed to it is measured against the deleted skill's
    /// install-time digest, and the compare link points at the deleted skill's
    /// upstream commit.
    ///
    /// A record is dropped only when the delete removed that skill's origin
    /// directory, which is the last thing [`Installer::delete`] does. `result`
    /// says whether it got there in both cases: an [`InstallError::Partial`]
    /// carries the changes that stood, so a delete that stopped on one of the
    /// links keeps the record of a skill that is still on disk rather than
    /// leaving it reporting [`LocalState::Unknown`] for ever.
    ///
    /// A skill whose origin is outside every managed scope keeps its record for
    /// the same reason: the delete took away the links, and the directory the
    /// digest describes is still there.
    ///
    /// An origin that was inside a managed scope and had already gone when the
    /// plan was made — [`DeletePlan::origin_missing`] — counts as removed.
    /// There is no directory left for the record to describe, so keeping it
    /// would hand it to the next skill of that name.
    ///
    /// The one case this leaves alone is a delete that changed nothing at all
    /// and whose origin was already gone: nothing happened, the user was told
    /// the delete was refused, and the record stays until a delete that does
    /// something drops it.
    pub fn forget_deleted(
        &mut self,
        plans: &[DeletePlan],
        result: &Result<Outcome, InstallError>,
    ) -> bool {
        let done = match result {
            Ok(outcome) => Cow::Borrowed(outcome),
            Err(error) => match error.completed() {
                Some(done) => done,
                // Nothing was changed, so nothing was removed.
                None => return false,
            },
        };
        let mut changed = false;
        for plan in plans {
            let removed = match &plan.origin {
                Some(origin) => origin_removed(&done.changes, origin),
                // Nothing for the delete to remove, so there is no change to
                // look for: the plan already says whether the directory was
                // gone or merely out of reach.
                None => plan.origin_missing,
            };
            if removed {
                changed |= self.forget_skill(&plan.name).is_some();
            }
        }
        changed
    }

    /// Moves a skill's record to a new name, which is what a rename should do.
    ///
    /// The records are keyed by skill name, so without this a renamed skill
    /// loses its digest and its shas: it reports [`LocalState::Unknown`] for
    /// ever, every update has to ask before overwriting, and the compare link
    /// the commit sha is kept for is gone. The record left behind under the old
    /// name is never collected, because nothing installs there any more.
    ///
    /// `digest` is the [`content_digest`] of the renamed directory, or `None`
    /// when it could not be computed. It is asked for rather than kept because
    /// a rename also rewrites the frontmatter `name`: the directory no longer
    /// hashes to what was recorded at install time, and carrying the old digest
    /// over would report a skill nobody touched as edited.
    ///
    /// Returns true when the cache changed and has to be written, which is the
    /// only question the caller has. Any record already under `to` is replaced
    /// when a record moves onto it, and dropped when there was nothing to move:
    /// it describes a skill that is no longer at that name, and calling the
    /// renamed directory pristine against someone else's digest is worse than
    /// having no answer. That drop is a change like any other, so it is
    /// reported and reaches the disk.
    pub fn rename_skill(&mut self, from: &str, to: &str, digest: Option<String>) -> bool {
        let changed = if from == to {
            self.skills.contains_key(from)
        } else {
            match self.skills.remove(from) {
                Some(record) => {
                    self.skills.insert(to.to_string(), record);
                    true
                }
                None => self.skills.remove(to).is_some(),
            }
        };
        if let Some(record) = self.skills.get_mut(to) {
            record.digest = digest.unwrap_or_default();
            record.recorded_at = now_unix();
        }
        changed
    }

    /// What the last check saw in one repository.
    pub fn repo(&self, key: &str) -> Option<&RepoRecord> {
        self.repos.get(key)
    }

    /// Records what a check saw in one repository.
    pub fn record_repo(&mut self, key: impl Into<String>, record: RepoRecord) {
        self.repos.insert(key.into(), record);
    }

    /// True when nothing at all is recorded.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty() && self.repos.is_empty()
    }
}

/// Why the cache at [`REMOTE_CACHE_FILE`] could not be written.
///
/// Every variant means the same thing to the user, whatever the cause: from now
/// on Skillbase has no record of what it installed. It cannot tell an edited
/// skill from an untouched one, so it has to ask before every update; and it
/// cannot remember what a check saw, so every check spends the whole hourly
/// GitHub budget again. Neither is worth stopping the app for, and neither is
/// something the user can work out on their own.
#[derive(Debug, Error)]
pub enum CacheWriteError {
    /// The cache could not be turned into JSON. A bug rather than a condition
    /// of the machine, but it is reported the same way.
    #[error("the install record could not be serialized: {0}")]
    Serialize(#[source] serde_json::Error),

    /// The file could not be written: no permission, no space, a read-only
    /// home, or `~/.skillbase` is not a directory.
    #[error("the install record at {path} could not be written: {source}")]
    Io {
        /// The path being created, written or renamed.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

impl CacheWriteError {
    /// Builds a [`CacheWriteError::Io`] carrying the path that failed.
    fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// The key a repository and ref are recorded under: `owner/repo@ref`.
pub fn repo_key(repo: &RepoRef) -> String {
    format!("{}@{}", repo.slug(), repo.reference)
}

/// Whether `changes` show that the directory at `origin` is no longer there.
///
/// [`Installer::delete`] moves a real directory into the trash, and reports
/// [`Change::NoChange`] for one that was not there when it looked. Either way
/// nothing of the skill is left at that path. A cross-filesystem move that
/// copied the directory and then could not remove the original reports
/// [`Change::Copied`], which is not the same thing: the skill is still where it
/// was, and its record still describes it.
fn origin_removed(changes: &[Change], origin: &Path) -> bool {
    changes.iter().any(|change| match change {
        Change::MovedToTrash { from, .. } => from == origin,
        Change::NoChange { path, .. } => path == origin,
        _ => false,
    })
}

/// Seconds since the Unix epoch, or 0 before it.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Local edits
// ---------------------------------------------------------------------------

/// Whether an installed skill still holds the bytes that were installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalState {
    /// Nothing was recorded for this skill, so the question cannot be answered
    /// from the cache. [`upstream_state`] answers it by downloading.
    Unknown,
    /// The directory matches what was installed.
    Pristine,
    /// The directory has changed since it was installed.
    Edited,
}

/// Whether `dir` still matches the digest recorded for `name`.
///
/// Reads the cache only, so it costs one directory hash and no network. A skill
/// with no record, and a directory that cannot be read, are both
/// [`LocalState::Unknown`]: neither is evidence of an edit.
pub fn local_state(cache: &RemoteCache, name: &str, dir: &Path) -> LocalState {
    let Some(record) = cache.skill(name).filter(|r| !r.digest.is_empty()) else {
        return LocalState::Unknown;
    };
    match content_digest(dir) {
        Ok(digest) if digest == record.digest => LocalState::Pristine,
        Ok(_) => LocalState::Edited,
        Err(_) => LocalState::Unknown,
    }
}

/// Whether `dir` matches upstream, worked out by downloading upstream.
///
/// The fallback for a lost cache, and the only answer available for a skill
/// another tool installed. One archive download, no API request beyond
/// resolving the ref and the subtree.
///
/// The extracted copy is compared twice: as it comes out of the archive, and
/// with this crate's provenance written into its `SKILL.md`. Matching either
/// counts as [`LocalState::Pristine`], because a skill `npx skills` installed
/// carries no provenance and one Skillbase installed carries it always, and
/// both are unedited.
pub fn upstream_state<H: Http>(
    gh: &GitHub<H>,
    roots: &Roots,
    location: &SkillLocation,
    dir: &Path,
) -> Result<LocalState, FetchError> {
    let local = content_digest(dir).map_err(|e| FetchError::io(dir, e))?;
    let staged = Staging::new(roots, "compare")?;
    let commit = gh.ref_sha(&location.repo)?;
    let tree_sha = gh.subtree_sha(&location.repo, &commit, &location.path)?;
    gh.fetch_files(
        &location.repo,
        &commit,
        &tree_sha,
        &location.path,
        staged.path(),
    )?;

    let bare = content_digest(staged.path()).map_err(|e| FetchError::io(staged.path(), e))?;
    if bare == local {
        return Ok(LocalState::Pristine);
    }
    write_provenance(staged.path(), &location.provenance(tree_sha))?;
    let stamped = content_digest(staged.path()).map_err(|e| FetchError::io(staged.path(), e))?;
    Ok(if stamped == local {
        LocalState::Pristine
    } else {
        LocalState::Edited
    })
}

// ---------------------------------------------------------------------------
// Update checking
// ---------------------------------------------------------------------------

/// One installed skill to check for an upstream update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateTarget {
    /// The installed skill's name, which is what results are keyed by.
    pub name: String,
    /// Where it came from.
    pub provenance: Provenance,
}

impl UpdateTarget {
    /// A target from a name and its provenance.
    pub fn new(name: impl Into<String>, provenance: Provenance) -> Self {
        Self {
            name: name.into(),
            provenance,
        }
    }

    /// The repository and ref to ask GitHub about, when the provenance names
    /// one that can be parsed.
    pub fn repo_ref(&self) -> Option<RepoRef> {
        let (owner, repo) = self.provenance.owner_repo()?;
        Some(RepoRef::new(owner, repo, &self.provenance.reference))
    }
}

/// Where an installed skill came from: its own frontmatter first, then the
/// `npx skills` lockfile.
///
/// The frontmatter wins because it travels with the skill and records a tree
/// sha; the lockfile records neither a sha nor a ref, so it can only say which
/// repository and path, and that is still enough to check for an update.
pub fn provenance_of(skill: &DiscoveredSkill, lock: &SkillLock) -> Option<Provenance> {
    skill
        .doc
        .as_ref()
        .and_then(|doc| Provenance::read(&doc.frontmatter))
        .or_else(|| lock.provenance(&skill.name))
}

/// Every skill that names where it came from, as targets for [`check_updates`].
///
/// A skill with no provenance at all is left out rather than reported as
/// unknown: it was written by hand or by a tool that records nothing, and
/// asking GitHub about it would be asking about nothing.
pub fn update_targets(skills: &[DiscoveredSkill], lock: &SkillLock) -> Vec<UpdateTarget> {
    skills
        .iter()
        .filter_map(|skill| {
            Some(UpdateTarget::new(
                skill.name.clone(),
                provenance_of(skill, lock)?,
            ))
        })
        .collect()
}

/// What a check found for one skill.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum UpdateStatus {
    /// The provenance does not name a GitHub repository, so there is nothing to
    /// check against.
    Unknown,
    /// Upstream's tree sha is the one recorded at install.
    UpToDate,
    /// Upstream's tree sha differs from the one recorded at install.
    UpdateAvailable {
        /// The tree sha upstream is at now.
        tree_sha: String,
    },
    /// The repository is reachable but records no baseline sha, which is the
    /// case for every skill `npx skills` installed. The current sha is
    /// reported, so a caller can record it, and [`upstream_state`] can settle
    /// the question by downloading.
    NoBaseline {
        /// The tree sha upstream is at now.
        tree_sha: String,
    },
    /// The repository no longer holds a directory at that path. The skill was
    /// renamed, moved or removed upstream.
    Gone,
    /// The check could not be made.
    Failed {
        /// What went wrong, in words.
        reason: String,
        /// True when the answer will not change until the rate limit resets,
        /// so retrying now is pointless.
        rate_limited: bool,
    },
}

impl UpdateStatus {
    /// True for [`UpdateStatus::UpdateAvailable`].
    pub fn is_update_available(&self) -> bool {
        matches!(self, Self::UpdateAvailable { .. })
    }
}

/// What one run of [`check_updates`] found.
#[derive(Debug, Clone, Default)]
pub struct UpdateReport {
    statuses: BTreeMap<String, UpdateStatus>,
    repos_checked: usize,
    requests: usize,
    rate_limit: Option<RateLimit>,
}

impl UpdateReport {
    /// The status of one skill, by name.
    pub fn status(&self, name: &str) -> Option<&UpdateStatus> {
        self.statuses.get(name)
    }

    /// Every status, ordered by skill name.
    pub fn statuses(&self) -> impl Iterator<Item = (&str, &UpdateStatus)> {
        self.statuses.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// The names with an update waiting, ordered.
    pub fn updatable(&self) -> impl Iterator<Item = &str> {
        self.statuses
            .iter()
            .filter(|(_, status)| status.is_update_available())
            .map(|(name, _)| name.as_str())
    }

    /// How many distinct repositories were asked about.
    pub fn repos_checked(&self) -> usize {
        self.repos_checked
    }

    /// How many HTTP requests the check made.
    pub fn requests(&self) -> usize {
        self.requests
    }

    /// What GitHub last said about the rate limit, so the interface can say
    /// "rate limited until 14:30" rather than failing blankly.
    pub fn rate_limit(&self) -> Option<RateLimit> {
        self.rate_limit
    }

    /// True when at least one skill failed because the limit is spent.
    pub fn hit_rate_limit(&self) -> bool {
        self.statuses.values().any(|status| {
            matches!(
                status,
                UpdateStatus::Failed {
                    rate_limited: true,
                    ..
                }
            )
        })
    }
}

/// Asks GitHub whether any of `targets` has moved on, batched by repository.
///
/// The funnel, cheapest first:
///
/// 1. Group the targets by repository and ref. Thirty-five installed skills
///    from thirteen repositories is thirteen groups.
/// 2. One `GET .../git/ref/heads/{branch}` per group. If the commit sha is the
///    one recorded last time and every path in the group is already in the
///    cache, the answers come from the cache and the group costs that single
///    request.
/// 3. Only a group whose commit moved walks trees, and the walk is shared
///    between every skill in it.
///
/// Never fails as a whole: a repository that cannot be reached marks its own
/// skills [`UpdateStatus::Failed`] and the rest are still checked. The one
/// exception is the rate limit, which stops the run — every further request
/// would get the same answer — and marks the skills not yet reached as failed
/// with `rate_limited` set.
///
/// `cache` is updated in place and is not written; the caller decides when to
/// persist it with [`RemoteCache::write`].
pub fn check_updates<H: Http>(
    gh: &GitHub<H>,
    targets: &[UpdateTarget],
    cache: &mut RemoteCache,
) -> UpdateReport {
    let before = gh.requests_made();
    let mut report = UpdateReport::default();

    // Group by repository and ref, keeping the targets of each group together.
    let mut groups: BTreeMap<RepoRef, Vec<&UpdateTarget>> = BTreeMap::new();
    for target in targets {
        match target.repo_ref() {
            Some(repo) => groups.entry(repo).or_default().push(target),
            None => {
                report
                    .statuses
                    .insert(target.name.clone(), UpdateStatus::Unknown);
            }
        }
    }

    let mut stopped: Option<GitHubError> = None;
    for (repo, group) in groups {
        if let Some(error) = &stopped {
            for target in group {
                report.statuses.insert(
                    target.name.clone(),
                    UpdateStatus::Failed {
                        reason: error.to_string(),
                        rate_limited: true,
                    },
                );
            }
            continue;
        }

        report.repos_checked += 1;
        match check_one_repo(gh, &repo, &group, cache) {
            Ok(statuses) => report.statuses.extend(statuses),
            Err(error) => {
                let rate_limited = error.is_rate_limited();
                for target in group {
                    report.statuses.insert(
                        target.name.clone(),
                        UpdateStatus::Failed {
                            reason: error.to_string(),
                            rate_limited,
                        },
                    );
                }
                if rate_limited {
                    stopped = Some(error);
                }
            }
        }
    }

    report.requests = gh.requests_made().saturating_sub(before);
    report.rate_limit = gh.rate_limit();
    report
}

/// One repository's share of [`check_updates`].
fn check_one_repo<H: Http>(
    gh: &GitHub<H>,
    repo: &RepoRef,
    group: &[&UpdateTarget],
    cache: &mut RemoteCache,
) -> Result<Vec<(String, UpdateStatus)>, GitHubError> {
    let key = repo_key(repo);
    let previous = cache.repo(&key).cloned().unwrap_or_default();
    // The ETag is only worth sending with a token; see `GitHub::ref_state`.
    let etag = Some(previous.etag.as_str())
        .filter(|etag| !etag.is_empty() && !previous.commit_sha.is_empty());
    let state = gh.ref_state(repo, etag)?;
    let commit = match state.sha {
        Some(sha) => sha,
        // 304: what the cache holds is current.
        None => previous.commit_sha.clone(),
    };
    let etag = state.etag.unwrap_or(previous.etag.clone());

    let mut paths: Vec<String> = group
        .iter()
        .map(|target| target.provenance.path.trim_matches('/').to_string())
        .collect();
    paths.sort();
    paths.dedup();

    // The whole point of step 1: an unchanged commit sha means no tree walk.
    let unchanged =
        previous.commit_sha == commit && paths.iter().all(|path| previous.trees.contains_key(path));
    let trees = if unchanged {
        previous.trees.clone()
    } else {
        let mut trees = gh.subtree_shas(repo, &commit, &paths)?;
        // Keep what an earlier check learned about paths not asked for now.
        if previous.commit_sha == commit {
            for (path, sha) in &previous.trees {
                trees.entry(path.clone()).or_insert_with(|| sha.clone());
            }
        }
        trees
    };

    cache.record_repo(
        &key,
        RepoRecord {
            commit_sha: commit,
            trees: trees.clone(),
            etag,
            checked_at: now_unix(),
        },
    );

    Ok(group
        .iter()
        .map(|target| {
            let path = target.provenance.path.trim_matches('/');
            let status = match trees.get(path) {
                None | Some(None) => UpdateStatus::Gone,
                Some(Some(upstream)) => match &target.provenance.tree_sha {
                    Some(installed) if installed == upstream => UpdateStatus::UpToDate,
                    Some(_) => UpdateStatus::UpdateAvailable {
                        tree_sha: upstream.clone(),
                    },
                    None => match cache.skill(&target.name).map(|r| &r.tree_sha) {
                        Some(recorded) if recorded == upstream => UpdateStatus::UpToDate,
                        Some(recorded) if !recorded.is_empty() => UpdateStatus::UpdateAvailable {
                            tree_sha: upstream.clone(),
                        },
                        _ => UpdateStatus::NoBaseline {
                            tree_sha: upstream.clone(),
                        },
                    },
                },
            };
            (target.name.clone(), status)
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Installing
// ---------------------------------------------------------------------------

/// Anything that can go wrong fetching a skill and writing it into the store.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FetchError {
    /// Talking to GitHub failed.
    #[error(transparent)]
    GitHub(#[from] GitHubError),

    /// Writing into the store failed, or was refused.
    #[error(transparent)]
    Install(#[from] InstallError),

    /// Reading or writing the downloaded `SKILL.md` failed.
    #[error(transparent)]
    Skill(#[from] SkillError),

    /// The downloaded directory holds no `SKILL.md`, so it is not a skill.
    #[error("{repo} has no SKILL.md at `{path}`, so there is no skill there")]
    NotASkill {
        /// The repository, as `owner/repo`.
        repo: String,
        /// The subdirectory that was downloaded.
        path: String,
    },

    /// No usable skill name could be worked out from the frontmatter or the
    /// directory name.
    #[error("`{name}` is not a usable skill name")]
    UnusableName {
        /// What was tried.
        name: String,
    },

    /// The install was stopped before anything was written into the store.
    #[error("the install was cancelled")]
    Cancelled,

    /// A filesystem call failed.
    #[error("{path}: {source}")]
    Io {
        /// The path being read or written.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },

    /// The install stopped after it had already changed the filesystem.
    ///
    /// Replacing an existing skill moves the old directory to the trash before
    /// the new one is put in its place. If the second half then fails, the
    /// skill the user had is only in the trash, under a timestamped name
    /// nothing else names. Carrying what was already done is what tells them
    /// where to find it.
    ///
    /// The counterpart of [`InstallError::Partial`], and it reads the same way:
    /// the changes that stand, then why the rest did not happen.
    ///
    /// Build it with [`FetchError::partial`], never by hand: an install that
    /// failed before it changed anything must stay the plain error.
    #[error("{}\nThen it stopped: {source}", done.describe())]
    Partial {
        /// What the install had already done.
        done: Box<Outcome>,
        /// Why it stopped.
        #[source]
        source: Box<FetchError>,
    },
}

impl FetchError {
    /// Builds a [`FetchError::Io`] carrying the path that failed.
    fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Attaches what the install had already done to the error that stopped it.
    ///
    /// An install that failed before it changed anything returns the plain
    /// error, so the ordinary refusal reads exactly as it always did.
    pub fn partial(done: Outcome, source: FetchError) -> Self {
        if done.is_noop() {
            return source;
        }
        Self::Partial {
            done: Box::new(done),
            source: Box::new(source),
        }
    }

    /// What the install had already done before it failed, when it had done
    /// anything.
    pub fn completed(&self) -> Option<&Outcome> {
        match self {
            Self::Partial { done, .. } => Some(done),
            _ => None,
        }
    }

    /// The message with `home` written as `~`, as
    /// [`InstallError::describe_under`].
    ///
    /// The abbreviation is presentation, so it is a second rendering rather
    /// than a change to the paths. It matters most for
    /// [`FetchError::Partial`]: the trash directory it names is what the user
    /// has to go and find, and `~/.skillbase/trash/pdf-1748...` is read where
    /// `/Users/someone/.skillbase/trash/pdf-1748...` is skimmed.
    pub fn describe_under(&self, home: &Path) -> String {
        match self {
            Self::Install(install) => install.describe_under(home),
            Self::Partial { done, source } => format!(
                "{}\nThen it stopped: {}",
                done.describe_under(home),
                source.describe_under(home)
            ),
            other => other.to_string(),
        }
    }
}

/// How to install.
#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    /// Install under this name instead of the one in the frontmatter.
    pub name: Option<String>,
    /// Replace a skill of the same name already in the store. Off by default,
    /// so installing never overwrites without being told to.
    pub replace: bool,
    /// Set from another thread to stop the install. Checked between steps, so
    /// a download already in flight still runs to its end, but nothing is
    /// written into the store after the flag is set.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl InstallOptions {
    /// The default: take the name from the skill, and refuse to overwrite.
    pub fn new() -> Self {
        Self::default()
    }

    /// Install under an explicit name.
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Replace a skill of the same name, which is what applying an update does.
    pub fn replacing(mut self) -> Self {
        self.replace = true;
        self
    }

    /// Stop the install when `flag` is set. The caller keeps a clone and sets
    /// it from the thread that owns the interface.
    pub fn cancelled_by(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancel = Some(flag);
        self
    }

    /// True once the cancel flag is set. False when there is no flag, which is
    /// the case for an install nobody can cancel.
    pub fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
    }
}

/// A skill downloaded from GitHub and written into the store.
#[derive(Debug, Clone)]
pub struct Installed {
    /// The name it was installed under, which is its directory name.
    pub name: String,
    /// Where it now is.
    pub dir: PathBuf,
    /// What was written into its frontmatter.
    pub provenance: Provenance,
    /// Its [`content_digest`] as installed, also recorded in the cache.
    ///
    /// Empty when the digest could not be computed. The skill is installed
    /// either way; what is lost is the ability to tell an edited copy from an
    /// untouched one, which [`local_state`] then reports as
    /// [`LocalState::Unknown`].
    pub digest: String,
    /// How many files were extracted.
    pub files: usize,
    /// What the sweep of [`STAGING_DIR`] this install ran first took away, and
    /// what it could not. See [`sweep_staging`].
    pub staging: StagingSweep,
    /// What changed on disk.
    pub outcome: Outcome,
}

/// Downloads one skill from GitHub and writes it into the store.
///
/// The store is `~/.agents/skills`, so an installed skill is visible to every
/// agent that reads that directory at once, with no symlink.
///
/// The sequence, and why:
///
/// 1. Resolve the ref to a commit sha, then the subdirectory to a tree sha.
///    The tree sha is what later answers "has upstream changed?", so it is
///    resolved before anything is downloaded rather than guessed after.
/// 2. Fetch the skill's own directory, pinned to that commit sha, into
///    [`STAGING_DIR`]. [`GitHub::fetch_files`] downloads the files one at a
///    time where it can, and only falls back to the whole repository archive
///    where it cannot.
/// 3. Write the provenance into the staged `SKILL.md`, under `metadata`.
/// 4. Move the staged directory into the store. Nothing appears in
///    `~/.agents/skills` until the skill is complete.
/// 5. Record the content digest, so a later check can tell an edited skill from
///    an untouched one, and the commit sha beside it, so an update can link at
///    the difference rather than only name it. The skill is in the store by
///    then, so a digest that cannot be taken leaves the record without one
///    rather than failing an install that worked.
///
/// The staging directory is removed whether this succeeds or fails. Before one
/// is made, [`sweep_staging`] takes away what runs that were killed mid-install
/// left behind; what it could not remove comes back in [`Installed::staging`].
///
/// # Cancelling
///
/// Every call here blocks, so the interface runs this on a background task.
/// Dropping that task does not stop a blocking call, which is why cancelling
/// is a flag rather than a drop: [`InstallOptions::cancel`] is read between
/// steps, and a set flag returns [`FetchError::Cancelled`]. The last check
/// comes immediately before the destination is touched, so a cancelled install
/// replaces nothing, trashes nothing, and leaves no staging directory behind.
/// A download already in flight still runs to its end; its bytes are thrown
/// away.
pub fn install_from_github<H: Http>(
    installer: &Installer,
    gh: &GitHub<H>,
    location: &SkillLocation,
    options: &InstallOptions,
    cache: &mut RemoteCache,
) -> Result<Installed, FetchError> {
    let roots = installer.roots();
    let commit = gh.ref_sha(&location.repo)?;
    let tree_sha = gh.subtree_sha(&location.repo, &commit, &location.path)?;
    if options.cancelled() {
        return Err(FetchError::Cancelled);
    }
    let staged = Staging::new(roots, location.dir_name())?;
    let files = gh.fetch_files(
        &location.repo,
        &commit,
        &tree_sha,
        &location.path,
        staged.path(),
    )?;
    if !staged.path().join(SKILL_FILE_NAME).is_file() {
        return Err(FetchError::NotASkill {
            repo: location.repo.slug(),
            path: location.path.clone(),
        });
    }

    let provenance = location.provenance(&tree_sha);
    let frontmatter_name = write_provenance(staged.path(), &provenance)?;

    let name = match &options.name {
        Some(name) => name.clone(),
        None => frontmatter_name
            .filter(|name| is_kebab_case(name))
            .unwrap_or_else(|| slugify(location.dir_name())),
    };
    if !is_kebab_case(&name) {
        return Err(FetchError::UnusableName { name });
    }

    // The last chance to stop, and the only one after the download: past this
    // point the destination is replaced and the staged directory is moved into
    // it. On this early return `Staging`'s `Drop` takes the staged directory
    // away, so a cancelled install leaves nothing anywhere.
    //
    // One check rather than two. A second one between the extraction and the
    // provenance write would save a single local file write, and nothing can
    // set the flag between two consecutive checks, so a test could only ever
    // reach the first of them — leaving this one, the one that matters,
    // uncovered.
    if options.cancelled() {
        return Err(FetchError::Cancelled);
    }

    let dest = installer.ensure_in_scope(&roots.store_dir().join(&name))?;
    let outcome = place_into_store(installer, staged.path(), &dest, options.replace)?;
    let staging = staged.keep();

    // The skill is on disk now, so a digest that cannot be computed is not a
    // failed install and is not reported as one. What is lost is edit
    // detection: the record goes in with an empty digest, which `local_state`
    // reads as "no baseline" rather than as an edit, and the tree and commit
    // shas an update check needs are still recorded.
    let digest = content_digest(&dest).unwrap_or_default();
    cache.record_skill(
        &name,
        SkillRecord {
            digest: digest.clone(),
            tree_sha: tree_sha.clone(),
            commit_sha: commit.clone(),
            recorded_at: now_unix(),
        },
    );

    Ok(Installed {
        name,
        dir: dest,
        provenance,
        digest,
        files,
        staging,
        outcome,
    })
}

/// Puts the staged directory at `dest`, taking whatever is already there to the
/// trash first.
///
/// The two halves are one step because only the first is destructive: between
/// them the skill the user had exists only under `~/.skillbase/trash`, in a
/// timestamped directory nothing else names. So a failure of the second half
/// comes back as [`FetchError::Partial`], carrying the [`Change::MovedToTrash`]
/// that says where the old skill went. A bare "Cross-device link" would leave
/// the user with an empty store entry and no idea their skill still existed.
fn place_into_store(
    installer: &Installer,
    staged: &Path,
    dest: &Path,
    replace: bool,
) -> Result<Outcome, FetchError> {
    let mut outcome = Outcome::default();
    if fs::symlink_metadata(dest).is_ok() {
        if !replace {
            return Err(InstallError::AlreadyExists {
                path: dest.to_path_buf(),
            }
            .into());
        }
        outcome
            .changes
            .extend(replace_existing(installer, dest)?.changes);
    }
    if let Some(parent) = dest.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        return Err(FetchError::partial(outcome, FetchError::io(parent, e)));
    }
    if let Err(e) = move_into_place(staged, dest) {
        return Err(FetchError::partial(outcome, e));
    }
    outcome.push(Change::CreatedDirectory {
        path: dest.to_path_buf(),
    });
    outcome.push(Change::WroteFile {
        path: dest.join(SKILL_FILE_NAME),
    });
    Ok(outcome)
}

/// Takes away what is already at `dest` before an install replaces it.
///
/// Goes through [`Installer::ensure_in_scope`] first, and refuses to follow a
/// symlink: a link at that path is unlinked, because a link holds no bytes of
/// its own and nothing is lost by removing it.
///
/// Anything else goes to [`Installer::remove_real_dir`], which moves the
/// directory into the trash rather than deleting it. The directory being
/// replaced may be a skill the person edited, and an update they did not want
/// has to be recoverable. That call does its own scope, symlink and
/// non-directory checks, and reports where the old directory landed.
fn replace_existing(installer: &Installer, dest: &Path) -> Result<Outcome, InstallError> {
    let dest = installer.ensure_in_scope(dest)?;
    let meta = fs::symlink_metadata(&dest).map_err(|e| InstallError::Io {
        path: dest.clone(),
        source: e,
    })?;
    if meta.file_type().is_symlink() {
        fs::remove_file(&dest).map_err(|e| InstallError::Io {
            path: dest.clone(),
            source: e,
        })?;
        return Ok(Outcome::one(Change::RemovedSymlink { path: dest }));
    }
    installer.remove_real_dir(&dest)
}

/// Moves the staged directory to `dest`, copying it across a filesystem
/// boundary.
///
/// The mirror of `Installer::move_to_trash`, which has had this fallback from
/// the start. Without it the destructive half of a replace can get across a
/// boundary and the constructive half cannot, so a store on another volume
/// would trash the old skill and then fail with "Cross-device link".
///
/// A copy that fails part-way leaves nothing at `dest`: half a skill in
/// `~/.agents/skills` is exactly what staging exists to prevent, and an agent
/// would load it.
fn move_into_place(from: &Path, dest: &Path) -> Result<(), FetchError> {
    let Err(rename_error) = fs::rename(from, dest) else {
        return Ok(());
    };
    if !crosses_filesystems(&rename_error) {
        return Err(FetchError::io(dest, rename_error));
    }
    if let Err(e) = copy_dir(from, dest) {
        let _ = fs::remove_dir_all(dest);
        return Err(e.into());
    }
    // The copy landed, so the skill is installed. A source that will not go is
    // one more directory under `STAGING_DIR` for the next sweep, not a failed
    // install, and reporting it as one would describe a disk that does not
    // exist.
    let _ = fs::remove_dir_all(from);
    Ok(())
}

/// Writes `provenance` into the `SKILL.md` in `dir` and returns the skill's
/// `name` from the frontmatter, when it has one.
///
/// The rest of the file is untouched: the body is never re-rendered, and every
/// other frontmatter key keeps its value and its position.
///
/// Rendered with [`SkillDoc::try_to_markdown`], not `to_markdown`: this
/// overwrites a file, and the lossy renderer would drop the one key YAML
/// refused and leave a `SKILL.md` that is well formed, missing a key, and
/// reported as a success. The name returned here comes from the in-memory
/// frontmatter, so it would name a key the file does not hold.
fn write_provenance(dir: &Path, provenance: &Provenance) -> Result<Option<String>, FetchError> {
    let path = dir.join(SKILL_FILE_NAME);
    let source = fs::read_to_string(&path).map_err(|e| FetchError::io(&path, e))?;
    let mut doc = SkillDoc::parse(&source)?;
    provenance.write(&mut doc.frontmatter);
    let text = doc.try_to_markdown()?;
    fs::write(&path, text).map_err(|e| FetchError::io(&path, e))?;
    Ok(doc.frontmatter.name().map(str::to_string))
}

// ---------------------------------------------------------------------------
// Staging
// ---------------------------------------------------------------------------

/// How old an entry under [`STAGING_DIR`] must be before a sweep takes it away.
///
/// Long enough that no install still running can be mistaken for abandoned —
/// including one started by a second copy of Skillbase, whose directories this
/// process cannot tell from its own — and short enough that a crash costs the
/// disk one extracted tarball rather than a growing pile of them.
const STALE_STAGING_AGE: Duration = Duration::from_secs(60 * 60);

/// What a sweep of [`STAGING_DIR`] did, from [`sweep_staging`].
///
/// `failed` is the part worth showing. Nothing else in Skillbase names
/// `~/.skillbase/staging` to the user, so a directory that cannot be removed
/// holds a whole extracted tarball, for ever, in a place nobody has been told
/// about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StagingSweep {
    removed: Vec<PathBuf>,
    failed: Vec<(PathBuf, String)>,
}

impl StagingSweep {
    /// What was taken away, in path order.
    pub fn removed(&self) -> &[PathBuf] {
        &self.removed
    }

    /// What could not be taken away, in path order, each with the reason.
    pub fn failed(&self) -> &[(PathBuf, String)] {
        &self.failed
    }

    /// True when nothing was left behind.
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty()
    }

    /// One sentence for the interface, or `None` when there is nothing to say.
    ///
    /// It names the directory, because that is the whole problem: the user
    /// cannot delete what nobody has told them about.
    pub fn warning(&self) -> Option<String> {
        let (path, reason) = self.failed.first()?;
        Some(match self.failed.len() {
            1 => format!(
                "A part-downloaded skill is still taking up space at {}, and could not be \
                 removed: {reason}. Delete it by hand to get the space back.",
                path.display()
            ),
            n => format!(
                "{n} part-downloaded skills are still taking up space in ~/{STAGING_DIR}, and \
                 could not be removed. The first is {}: {reason}. Delete them by hand to get \
                 the space back.",
                path.display()
            ),
        })
    }
}

/// Takes away everything under [`STAGING_DIR`] that an earlier run abandoned.
///
/// A staging directory is removed when its [`Staging`] guard drops, so this
/// only ever finds the ones where that never happened: the process was killed,
/// or the machine lost power, mid-install. Each holds a fully extracted
/// tarball.
///
/// Sweeping rather than reporting the drop is deliberate. The failure the user
/// actually meets is the one no `Drop` ever ran for, so there is no error to
/// report — only a directory nobody will look in again. Reporting is kept for
/// what the sweep itself could not remove, in [`StagingSweep::warning`].
///
/// Only entries older than [`STALE_STAGING_AGE`] are touched, so an install
/// running right now — in this process or another copy of Skillbase — is left
/// alone.
pub fn sweep_staging(roots: &Roots) -> StagingSweep {
    sweep_staging_dir(&roots.home().join(STAGING_DIR), STALE_STAGING_AGE)
}

fn sweep_staging_dir(root: &Path, older_than: Duration) -> StagingSweep {
    let mut sweep = StagingSweep::default();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        // Nothing has ever been staged. The ordinary case, not a failure.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return sweep,
        Err(e) => {
            sweep.failed.push((root.to_path_buf(), e.to_string()));
            return sweep;
        }
    };

    let now = SystemTime::now();
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(e) => {
                sweep.failed.push((root.to_path_buf(), e.to_string()));
                continue;
            }
        };
        match is_stale(&path, now, older_than) {
            Err(e) => sweep.failed.push((path, e.to_string())),
            Ok(false) => {}
            Ok(true) => match remove_staged(&path) {
                Ok(()) => sweep.removed.push(path),
                Err(e) => sweep.failed.push((path, e.to_string())),
            },
        }
    }

    sweep.removed.sort();
    sweep.failed.sort();
    sweep
}

/// Whether `path` was last written more than `older_than` ago. A modification
/// time in the future reads as fresh, because the alternative is deleting a
/// directory on the strength of a wrong clock.
fn is_stale(path: &Path, now: SystemTime, older_than: Duration) -> Result<bool, io::Error> {
    let modified = fs::symlink_metadata(path)?.modified()?;
    Ok(now
        .duration_since(modified)
        .is_ok_and(|age| age >= older_than))
}

/// Removes one entry under [`STAGING_DIR`], whatever it turned out to be.
fn remove_staged(path: &Path) -> Result<(), io::Error> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// A directory under [`STAGING_DIR`], removed when it goes out of scope unless
/// it was kept.
///
/// A download is assembled here rather than in the store, so an interrupted
/// install leaves nothing an agent could load.
struct Staging {
    path: PathBuf,
    keep: bool,
    /// What the sweep this staging directory ran on the way in found. Carried
    /// out through [`Installed::staging`].
    sweep: StagingSweep,
}

impl Staging {
    /// Creates a staging directory whose name will not collide with a
    /// concurrent install, sweeping abandoned ones first.
    fn new(roots: &Roots, label: &str) -> Result<Staging, FetchError> {
        let root = roots.home().join(STAGING_DIR);
        fs::create_dir_all(&root).map_err(|e| FetchError::io(&root, e))?;
        // Before anything is added, take away what earlier runs never did.
        // An install is the only moment this directory is thought about, so it
        // is the only moment the pile can be found.
        let sweep = sweep_staging_dir(&root, STALE_STAGING_AGE);

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let label = slugify(label);
        let path = root.join(format!("{label}-{}-{unique}", std::process::id()));
        // A leftover from a crashed run under the same name is not reused: its
        // files would be extracted over and shipped into the store. The name
        // carries a process id and a nanosecond, so this all but never fires —
        // and when it does, a download that cannot get a clean directory is a
        // download that must stop.
        if let Err(e) = fs::remove_dir_all(&path)
            && e.kind() != io::ErrorKind::NotFound
        {
            return Err(FetchError::io(&path, e));
        }
        fs::create_dir_all(&path).map_err(|e| FetchError::io(&path, e))?;
        Ok(Staging {
            path,
            keep: false,
            sweep,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// The directory has been moved away, so there is nothing left to remove.
    fn keep(mut self) -> StagingSweep {
        self.keep = true;
        std::mem::take(&mut self.sweep)
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.keep {
            // Nowhere to report a failure from here, which is why the sweep
            // exists: whatever this leaves behind, the next install finds.
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::github::tests_support::tarball;
    use crate::http::HttpResponse;
    use crate::http::fake::FakeHttp;
    use crate::test_fixture::Fixture;

    fn write(dir: &Path, relative: &str, contents: &str) {
        let path = dir.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    // -- digest --------------------------------------------------------------

    #[test]
    fn the_digest_is_stable_and_changes_when_a_file_changes() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("skill");
        write(&dir, "SKILL.md", "---\nname: pdf\n---\n# pdf\n");
        write(&dir, "scripts/run.sh", "#!/bin/sh\n");

        let first = content_digest(&dir).unwrap();
        assert_eq!(first, content_digest(&dir).unwrap(), "stable");
        assert_eq!(first.len(), 64);

        write(&dir, "scripts/run.sh", "#!/bin/sh\necho hi\n");
        let after_edit = content_digest(&dir).unwrap();
        assert_ne!(first, after_edit, "an edited file changes the digest");

        write(&dir, "references/notes.md", "notes\n");
        assert_ne!(
            after_edit,
            content_digest(&dir).unwrap(),
            "a new file changes the digest"
        );
    }

    #[test]
    fn the_digest_covers_names_as_well_as_contents() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        write(&a, "one.txt", "x");
        write(&a, "two.txt", "y");
        write(&b, "one.txt", "y");
        write(&b, "two.txt", "x");
        assert_ne!(content_digest(&a).unwrap(), content_digest(&b).unwrap());
    }

    #[test]
    fn a_symlink_is_hashed_by_its_target_and_never_followed() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("skill");
        write(&dir, "SKILL.md", "x\n");
        write(temp.path(), "outside.txt", "secret\n");
        symlink("../outside.txt", dir.join("link")).unwrap();

        let first = content_digest(&dir).unwrap();
        fs::write(temp.path().join("outside.txt"), "changed\n").unwrap();
        assert_eq!(
            first,
            content_digest(&dir).unwrap(),
            "the target's content is not read"
        );

        fs::remove_file(dir.join("link")).unwrap();
        symlink("../elsewhere.txt", dir.join("link")).unwrap();
        assert_ne!(first, content_digest(&dir).unwrap(), "the target text is");
    }

    // -- cache ---------------------------------------------------------------

    #[test]
    fn the_cache_round_trips_and_a_bad_one_reads_as_empty() {
        let fx = Fixture::empty();
        let mut cache = RemoteCache::new();
        cache.record_skill(
            "pdf",
            SkillRecord {
                digest: "abc".into(),
                tree_sha: "def".into(),
                commit_sha: "c0ffee".into(),
                recorded_at: 7,
            },
        );
        assert!(
            cache.write(&fx.roots()).is_none(),
            "a write that worked reports nothing"
        );
        let read = RemoteCache::read(&fx.roots());
        assert_eq!(read.skill("pdf").unwrap().digest, "abc");
        assert_eq!(read.skill("pdf").unwrap().commit_sha, "c0ffee");

        fx.write_file(REMOTE_CACHE_FILE, "{ not json ");
        assert!(RemoteCache::read(&fx.roots()).is_empty());

        fx.write_file(REMOTE_CACHE_FILE, r#"{"version":99,"skills":{"pdf":{}}}"#);
        assert!(RemoteCache::read(&fx.roots()).is_empty());

        assert!(RemoteCache::read_file(Path::new("/nonexistent/remote.json")).is_empty());
    }

    /// A renamed skill keeps its record, so it can still be called pristine and
    /// an update can still link at the difference.
    #[test]
    fn a_renamed_skill_takes_its_record_with_it() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("pdf-tools");
        write(&dir, "SKILL.md", "---\nname: pdf-tools\n---\n");

        let mut cache = RemoteCache::new();
        cache.record_skill(
            "pdf",
            SkillRecord {
                digest: "the-digest-before-the-rename".into(),
                tree_sha: "pdf-tree-sha".into(),
                commit_sha: "c0ffee".into(),
                recorded_at: 7,
            },
        );

        assert!(cache.rename_skill("pdf", "pdf-tools", content_digest(&dir).ok()));

        assert!(
            cache.skill("pdf").is_none(),
            "nothing is left under the old name to go stale"
        );
        let record = cache.skill("pdf-tools").expect("the record moved");
        assert_eq!(record.tree_sha, "pdf-tree-sha");
        assert_eq!(record.commit_sha, "c0ffee");
        // The frontmatter `name` was rewritten by the rename, so the digest had
        // to be taken again; keeping the old one would read as an edit.
        assert_eq!(local_state(&cache, "pdf-tools", &dir), LocalState::Pristine);
    }

    #[test]
    fn renaming_over_a_name_that_has_a_record_does_not_leave_the_old_one_answering() {
        let mut cache = RemoteCache::new();
        cache.record_skill(
            "pdf-tools",
            SkillRecord {
                digest: "someone-elses".into(),
                tree_sha: "other".into(),
                ..SkillRecord::default()
            },
        );

        // Nothing was recorded for `pdf`, so there is nothing to move, and the
        // record sitting under the new name describes a skill that is not there
        // any more. Dropping it is a change like any other, so the caller is
        // told to write the cache.
        assert!(cache.rename_skill("pdf", "pdf-tools", Some("new".into())));
        assert!(cache.skill("pdf-tools").is_none());
        assert!(cache.is_empty());
    }

    /// The call site writes the cache only when the rename says the cache
    /// changed, so a drop that is not reported never reaches the disk and the
    /// stale record answers again on the next read.
    #[test]
    fn the_stale_record_a_rename_drops_stays_dropped() {
        let fx = Fixture::empty();
        let mut cache = RemoteCache::new();
        cache.record_skill(
            "pdf-tools",
            SkillRecord {
                digest: "the-deleted-skills-digest".into(),
                commit_sha: "c0ffee".into(),
                ..SkillRecord::default()
            },
        );
        cache.write(&fx.roots());

        let mut cache = RemoteCache::read(&fx.roots());
        if cache.rename_skill("pdf", "pdf-tools", Some("new".into())) {
            cache.write(&fx.roots());
        }

        assert!(
            RemoteCache::read(&fx.roots()).skill("pdf-tools").is_none(),
            "the drop was written, so nothing measures the renamed directory \
             against a deleted skill's digest"
        );
    }

    #[test]
    fn a_rename_with_nothing_recorded_on_either_side_changes_nothing() {
        let mut cache = RemoteCache::new();
        cache.record_skill("unrelated", SkillRecord::default());

        assert!(!cache.rename_skill("pdf", "pdf-tools", Some("new".into())));
        assert!(cache.skill("unrelated").is_some());
    }

    #[test]
    fn renaming_a_skill_to_the_name_it_already_has_only_refreshes_the_digest() {
        let mut cache = RemoteCache::new();
        cache.record_skill(
            "pdf",
            SkillRecord {
                digest: "before".into(),
                tree_sha: "pdf-tree-sha".into(),
                ..SkillRecord::default()
            },
        );

        assert!(cache.rename_skill("pdf", "pdf", Some("after".into())));
        let record = cache.skill("pdf").expect("still there");
        assert_eq!(record.digest, "after");
        assert_eq!(record.tree_sha, "pdf-tree-sha");
    }

    #[test]
    fn a_rename_with_no_digest_leaves_no_baseline_rather_than_a_wrong_one() {
        let mut cache = RemoteCache::new();
        cache.record_skill(
            "pdf",
            SkillRecord {
                digest: "before".into(),
                tree_sha: "pdf-tree-sha".into(),
                ..SkillRecord::default()
            },
        );

        assert!(cache.rename_skill("pdf", "pdf-tools", None));
        assert!(cache.skill("pdf-tools").unwrap().digest.is_empty());
        assert_eq!(cache.skill("pdf-tools").unwrap().tree_sha, "pdf-tree-sha");
    }

    /// A record left behind by a delete answers for whatever takes the name
    /// next, because the records are keyed by name.
    #[test]
    fn a_deleted_skill_takes_its_record_with_it() {
        let fx = Fixture::realistic();
        let dir = fx.shared().join("shared-one");

        let mut cache = RemoteCache::new();
        cache.record_skill(
            "shared-one",
            SkillRecord {
                digest: content_digest(&dir).unwrap(),
                tree_sha: "shared-one-tree-sha".into(),
                commit_sha: "c0ffee".into(),
                recorded_at: 7,
            },
        );

        let installer = fx.installer();
        let plan = installer.plan_delete(fx.scan().get("shared-one").unwrap());
        let result = installer.delete(&plan);
        assert!(result.is_ok(), "{result:?}");

        assert!(
            cache.forget_deleted(std::slice::from_ref(&plan), &result),
            "the cache changed, so the caller has to write it"
        );
        assert!(cache.skill("shared-one").is_none());

        // A local skill written at the same name afterwards holds the same
        // bytes the deleted one did, so the record left behind would have
        // called a skill nobody installed pristine.
        let fresh = fx.skill(".agents/skills/shared-one", "shared-one");
        assert_eq!(
            local_state(&cache, "shared-one", &fresh),
            LocalState::Unknown
        );
    }

    /// The other half: a delete that stopped leaves the skill on disk, and a
    /// record dropped there would report it as having no install record for
    /// ever.
    #[test]
    fn a_delete_that_never_reached_the_directory_keeps_its_record() {
        let fx = Fixture::realistic();
        let dir = fx.shared().join("shared-one");

        let mut cache = RemoteCache::new();
        cache.record_skill(
            "shared-one",
            SkillRecord {
                digest: content_digest(&dir).unwrap(),
                ..SkillRecord::default()
            },
        );

        // The second link is a real directory, so the delete refuses it after
        // the first link is gone and before it reaches the origin.
        let plan = DeletePlan {
            name: "shared-one".into(),
            origin: Some(dir.clone()),
            links: vec![
                fx.agent("claude-code").join("shared-one"),
                fx.agent("claude-code").join("claude-only"),
            ],
            ..DeletePlan::default()
        };
        let result = fx.installer().delete(&plan);
        assert!(result.is_err(), "{result:?}");
        assert!(dir.is_dir(), "the skill is still where it was");

        assert!(!cache.forget_deleted(std::slice::from_ref(&plan), &result));
        assert_eq!(
            local_state(&cache, "shared-one", &dir),
            LocalState::Pristine
        );
    }

    /// A skill whose directory went away before the delete was planned is as
    /// gone as one the delete removed itself, and its record would answer for
    /// the next skill of that name just the same.
    #[test]
    fn a_delete_whose_origin_had_already_gone_forgets_the_record() {
        let fx = Fixture::realistic();
        let dir = fx.shared().join("shared-one");

        let mut cache = RemoteCache::new();
        cache.record_skill(
            "shared-one",
            SkillRecord {
                digest: content_digest(&dir).unwrap(),
                ..SkillRecord::default()
            },
        );

        let installer = fx.installer();
        let scan = fx.scan();
        // Removed by something other than Skillbase between the scan and the
        // plan. The scan is what the interface was looking at when the user
        // pressed Delete.
        fs::remove_dir_all(&dir).unwrap();

        let plan = installer.plan_delete(scan.get("shared-one").unwrap());
        assert_eq!(plan.origin, None, "there is nothing left to remove");
        let result = installer.delete(&plan);
        assert!(result.is_ok(), "{result:?}");

        assert!(cache.forget_deleted(std::slice::from_ref(&plan), &result));
        assert!(cache.skill("shared-one").is_none());
    }

    /// The other origin a plan leaves empty: one outside every managed scope.
    /// The delete never reaches it, so the directory the digest describes is
    /// still there and the record still describes it.
    #[test]
    fn a_delete_that_left_an_unmanaged_origin_alone_keeps_the_record() {
        let fx = Fixture::realistic();
        let dir = fx.home().join("Documents/secret");

        let mut cache = RemoteCache::new();
        cache.record_skill(
            "secret",
            SkillRecord {
                digest: content_digest(&dir).unwrap(),
                ..SkillRecord::default()
            },
        );

        // What `plan_delete` produces for an origin outside every scope: no
        // origin, the path listed as left alone, and the links still going.
        let plan = DeletePlan {
            name: "secret".into(),
            origin: None,
            origin_missing: false,
            links: vec![fx.agent("claude-code").join("shared-one")],
            skipped: vec![dir.clone()],
            ..DeletePlan::default()
        };
        let result = fx.installer().delete(&plan);
        assert!(result.is_ok(), "{result:?}");
        assert!(dir.is_dir(), "the origin was never in reach");

        assert!(!cache.forget_deleted(std::slice::from_ref(&plan), &result));
        assert_eq!(local_state(&cache, "secret", &dir), LocalState::Pristine);
    }

    /// Deleting a marked set runs the plans in order and stops at the first
    /// refusal, so one result covers skills that went and skills that stayed.
    #[test]
    fn a_bulk_delete_forgets_only_the_skills_it_removed() {
        let fx = Fixture::realistic();
        let mut cache = RemoteCache::new();
        for name in ["shared-one", "shared-two"] {
            cache.record_skill(
                name,
                SkillRecord {
                    digest: content_digest(&fx.shared().join(name)).unwrap(),
                    ..SkillRecord::default()
                },
            );
        }

        let installer = fx.installer();
        let scan = fx.scan();
        let plans = vec![
            installer.plan_delete(scan.get("shared-one").unwrap()),
            installer.plan_delete(scan.get("shared-two").unwrap()),
        ];
        let done = installer.delete(&plans[0]).unwrap();
        let result = Err(InstallError::partial(
            done,
            InstallError::NotALink {
                path: fx.agent("claude-code").join("claude-only"),
                kind: "directory",
            },
        ));

        assert!(cache.forget_deleted(&plans, &result));
        assert!(cache.skill("shared-one").is_none(), "removed");
        assert!(
            cache.skill("shared-two").is_some(),
            "never touched, so still recorded"
        );
    }

    #[test]
    fn a_cache_that_cannot_be_written_reports_the_failure() {
        let fx = Fixture::empty();
        // `~/.skillbase` is a file, so the directory the cache lives in cannot
        // be created. A read-only home reads the same way.
        fx.write_file(".skillbase", "not a directory\n");

        let mut cache = RemoteCache::new();
        cache.record_skill("pdf", SkillRecord::default());
        let error = cache
            .write(&fx.roots())
            .expect("a write that failed reports why");

        assert!(matches!(error, CacheWriteError::Io { .. }), "{error:?}");
        let sentence = error.to_string();
        assert!(sentence.contains("install record"), "{sentence}");
        assert!(sentence.contains(".skillbase"), "{sentence}");

        // And the consequence the user would otherwise never be told about:
        // there is no record, so nothing can be called pristine.
        assert!(RemoteCache::read(&fx.roots()).is_empty());
    }

    #[test]
    fn local_state_reads_the_digest_and_says_unknown_without_one() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("pdf");
        write(&dir, "SKILL.md", "x\n");

        let mut cache = RemoteCache::new();
        assert_eq!(local_state(&cache, "pdf", &dir), LocalState::Unknown);

        cache.record_skill(
            "pdf",
            SkillRecord {
                digest: content_digest(&dir).unwrap(),
                tree_sha: "t".into(),
                ..SkillRecord::default()
            },
        );
        assert_eq!(local_state(&cache, "pdf", &dir), LocalState::Pristine);

        write(&dir, "SKILL.md", "edited\n");
        assert_eq!(local_state(&cache, "pdf", &dir), LocalState::Edited);
    }

    // -- update checking -----------------------------------------------------

    /// Two repositories, one holding two skills.
    fn update_http() -> FakeHttp {
        let http = FakeHttp::new();
        http.json(
            "https://api.test/repos/o/one/git/ref/heads/main",
            r#"{"object":{"sha":"commit-one","type":"commit"}}"#,
        );
        http.json(
            "https://api.test/repos/o/two/git/ref/heads/main",
            r#"{"object":{"sha":"commit-two","type":"commit"}}"#,
        );
        http.json(
            "https://api.test/repos/o/one/git/trees/commit-one",
            r#"{"sha":"root1","truncated":false,"tree":[
                {"path":"skills","type":"tree","sha":"skills1"}]}"#,
        );
        http.json(
            "https://api.test/repos/o/one/git/trees/skills1",
            r#"{"sha":"skills1","truncated":false,"tree":[
                {"path":"pdf","type":"tree","sha":"pdf-new"},
                {"path":"docx","type":"tree","sha":"docx-same"}]}"#,
        );
        http.json(
            "https://api.test/repos/o/two/git/trees/commit-two",
            r#"{"sha":"root2","truncated":false,"tree":[
                {"path":"solo","type":"tree","sha":"solo-same"}]}"#,
        );
        http
    }

    fn client(http: FakeHttp) -> GitHub<FakeHttp> {
        GitHub::new(http).with_endpoints(
            "https://api.test",
            "https://codeload.test",
            "https://raw.test",
        )
    }

    fn target(name: &str, repo: &str, path: &str, sha: Option<&str>) -> UpdateTarget {
        UpdateTarget::new(
            name,
            Provenance {
                repo_url: format!("https://github.com/{repo}"),
                reference: "main".into(),
                tree_sha: sha.map(str::to_string),
                path: path.into(),
            },
        )
    }

    #[test]
    fn checking_batches_by_repository_and_reports_each_skill() {
        let gh = client(update_http());
        let targets = vec![
            target("pdf", "o/one", "skills/pdf", Some("pdf-old")),
            target("docx", "o/one", "skills/docx", Some("docx-same")),
            target("solo", "o/two", "solo", Some("solo-same")),
            target("gone", "o/two", "vanished", Some("whatever")),
            UpdateTarget::new(
                "local",
                Provenance {
                    repo_url: String::new(),
                    reference: "main".into(),
                    tree_sha: None,
                    path: String::new(),
                },
            ),
        ];
        let mut cache = RemoteCache::new();
        let report = check_updates(&gh, &targets, &mut cache);

        assert_eq!(
            report.status("pdf"),
            Some(&UpdateStatus::UpdateAvailable {
                tree_sha: "pdf-new".into()
            })
        );
        assert_eq!(report.status("docx"), Some(&UpdateStatus::UpToDate));
        assert_eq!(report.status("solo"), Some(&UpdateStatus::UpToDate));
        assert_eq!(report.status("gone"), Some(&UpdateStatus::Gone));
        assert_eq!(report.status("local"), Some(&UpdateStatus::Unknown));
        assert_eq!(report.updatable().collect::<Vec<_>>(), ["pdf"]);
        assert_eq!(report.repos_checked(), 2);

        // Two refs, two root trees, one subtree: five, not one per skill.
        assert_eq!(gh.http().request_count(), 5);
    }

    #[test]
    fn an_unchanged_ref_sha_skips_the_tree_walk_entirely() {
        let gh = client(update_http());
        let targets = vec![
            target("pdf", "o/one", "skills/pdf", Some("pdf-old")),
            target("docx", "o/one", "skills/docx", Some("docx-same")),
        ];
        let mut cache = RemoteCache::new();
        check_updates(&gh, &targets, &mut cache);
        let first = gh.http().request_count();
        assert!(first > 1);

        let second = check_updates(&gh, &targets, &mut cache);
        assert_eq!(
            gh.http().request_count() - first,
            1,
            "one request for the repository, and no tree walk"
        );
        assert_eq!(second.requests(), 1);
        assert_eq!(
            second.status("pdf"),
            Some(&UpdateStatus::UpdateAvailable {
                tree_sha: "pdf-new".into()
            })
        );
        assert_eq!(second.status("docx"), Some(&UpdateStatus::UpToDate));
    }

    #[test]
    fn a_moved_ref_sha_walks_the_trees_again() {
        let http = update_http();
        let gh = client(http);
        let targets = vec![target("pdf", "o/one", "skills/pdf", Some("pdf-old"))];
        let mut cache = RemoteCache::new();
        check_updates(&gh, &targets, &mut cache);

        // The branch moves, and the new commit's trees say something else.
        gh.http().json(
            "https://api.test/repos/o/one/git/ref/heads/main",
            r#"{"object":{"sha":"commit-later","type":"commit"}}"#,
        );
        gh.http().json(
            "https://api.test/repos/o/one/git/trees/commit-later",
            r#"{"sha":"root1b","truncated":false,"tree":[
                {"path":"skills","type":"tree","sha":"skills1b"}]}"#,
        );
        gh.http().json(
            "https://api.test/repos/o/one/git/trees/skills1b",
            r#"{"sha":"skills1b","truncated":false,"tree":[
                {"path":"pdf","type":"tree","sha":"pdf-old"}]}"#,
        );
        let report = check_updates(&gh, &targets, &mut cache);
        assert_eq!(report.status("pdf"), Some(&UpdateStatus::UpToDate));
        assert_eq!(report.requests(), 3);
    }

    #[test]
    fn a_skill_from_the_lockfile_has_no_baseline_until_one_is_recorded() {
        let gh = client(update_http());
        let targets = vec![target("pdf", "o/one", "skills/pdf", None)];
        let mut cache = RemoteCache::new();
        let report = check_updates(&gh, &targets, &mut cache);
        assert_eq!(
            report.status("pdf"),
            Some(&UpdateStatus::NoBaseline {
                tree_sha: "pdf-new".into()
            })
        );

        cache.record_skill(
            "pdf",
            SkillRecord {
                digest: String::new(),
                tree_sha: "pdf-old".into(),
                ..SkillRecord::default()
            },
        );
        let report = check_updates(&gh, &targets, &mut cache);
        assert!(report.status("pdf").unwrap().is_update_available());
    }

    #[test]
    fn a_rate_limited_check_stops_and_names_the_reset_time() {
        let http = FakeHttp::new();
        http.pattern(
            "/git/ref/heads/",
            HttpResponse::new(403, br#"{"message":"API rate limit exceeded"}"#.to_vec())
                .with_header("x-ratelimit-limit", "60")
                .with_header("x-ratelimit-remaining", "0")
                .with_header("x-ratelimit-reset", "1800000000"),
        );
        let gh = client(http);
        let targets = vec![
            target("pdf", "o/one", "skills/pdf", Some("a")),
            target("solo", "o/two", "solo", Some("b")),
        ];
        let mut cache = RemoteCache::new();
        let report = check_updates(&gh, &targets, &mut cache);

        assert!(report.hit_rate_limit());
        for name in ["pdf", "solo"] {
            match report.status(name).unwrap() {
                UpdateStatus::Failed {
                    reason,
                    rate_limited,
                } => {
                    assert!(*rate_limited, "{name}");
                    assert!(reason.contains("rate limit"), "{reason}");
                }
                other => panic!("{name}: {other:?}"),
            }
        }
        // The second repository was never asked: one request, then a stop.
        assert_eq!(gh.http().request_count(), 1);
        let limit = report.rate_limit().expect("headers were read");
        assert_eq!(limit.reset_unix, 1_800_000_000);
        assert!(limit.is_exhausted());
    }

    #[test]
    fn one_unreachable_repository_does_not_hide_the_others() {
        let http = update_http();
        http.reply(
            "https://api.test/repos/o/two/git/ref/heads/main",
            HttpResponse::new(500, b"boom".to_vec()),
        );
        let gh = client(http);
        let targets = vec![
            target("pdf", "o/one", "skills/pdf", Some("pdf-old")),
            target("solo", "o/two", "solo", Some("solo-same")),
        ];
        let mut cache = RemoteCache::new();
        let report = check_updates(&gh, &targets, &mut cache);
        assert!(report.status("pdf").unwrap().is_update_available());
        assert!(matches!(
            report.status("solo"),
            Some(UpdateStatus::Failed {
                rate_limited: false,
                ..
            })
        ));
    }

    #[test]
    fn targets_come_from_the_frontmatter_first_and_the_lockfile_second() {
        let fx = Fixture::empty();
        fx.installed_skill(
            ".agents/skills/pdf",
            "pdf",
            "anthropics/skills",
            "document-skills/pdf",
            "pdf-old",
        );
        // Installed by `npx skills`: no provenance in the file, an entry in the
        // lockfile instead.
        fx.skill(".agents/skills/docx", "docx");
        // Written by hand: nothing says where it came from.
        fx.skill(".agents/skills/mine", "mine");
        fx.skill_lock(&[("docx", "someone/agent-skills", "docx")]);

        let found = fx.scan();
        let lock = SkillLock::read(&fx.roots());
        let targets = update_targets(&found.skills, &lock);

        assert_eq!(
            targets
                .iter()
                .map(|target| target.name.as_str())
                .collect::<Vec<_>>(),
            ["docx", "pdf"]
        );
        let pdf = targets.iter().find(|t| t.name == "pdf").unwrap();
        assert_eq!(pdf.provenance.tree_sha.as_deref(), Some("pdf-old"));
        assert_eq!(pdf.repo_ref().unwrap().slug(), "anthropics/skills");

        let docx = targets.iter().find(|t| t.name == "docx").unwrap();
        assert_eq!(docx.provenance.tree_sha, None);
        assert_eq!(docx.repo_ref().unwrap().slug(), "someone/agent-skills");
    }

    #[test]
    fn a_token_makes_the_next_ref_request_conditional() {
        let http = FakeHttp::new();
        http.reply(
            "https://api.test/repos/o/one/git/ref/heads/main",
            HttpResponse::new(
                200,
                br#"{"object":{"sha":"commit-one","type":"commit"}}"#.to_vec(),
            )
            .with_header("etag", "W/\"abc\""),
        );
        http.json(
            "https://api.test/repos/o/one/git/trees/commit-one",
            r#"{"sha":"root1","truncated":false,"tree":[
                {"path":"pdf","type":"tree","sha":"pdf-new"}]}"#,
        );
        let gh = client(http).with_token(Some("t".into()));
        let targets = vec![target("pdf", "o/one", "pdf", Some("pdf-old"))];
        let mut cache = RemoteCache::new();

        check_updates(&gh, &targets, &mut cache);
        assert_eq!(cache.repo("o/one@main").unwrap().etag, "W/\"abc\"");
        assert!(
            gh.http().requests()[0]
                .headers
                .iter()
                .all(|(name, _)| name != "If-None-Match"),
            "nothing to be conditional about yet"
        );

        // GitHub says nothing changed, and that costs no quota with a token.
        gh.http().reply(
            "https://api.test/repos/o/one/git/ref/heads/main",
            HttpResponse::new(304, Vec::new()),
        );
        let report = check_updates(&gh, &targets, &mut cache);
        assert!(report.status("pdf").unwrap().is_update_available());
        let last = gh.http().requests().pop().unwrap();
        assert!(
            last.headers
                .iter()
                .any(|(name, value)| name == "If-None-Match" && value == "W/\"abc\""),
        );
    }

    #[test]
    fn without_a_token_no_etag_is_sent_because_a_304_still_costs_a_request() {
        let http = FakeHttp::new();
        http.reply(
            "https://api.test/repos/o/one/git/ref/heads/main",
            HttpResponse::new(
                200,
                br#"{"object":{"sha":"commit-one","type":"commit"}}"#.to_vec(),
            )
            .with_header("etag", "W/\"abc\""),
        );
        http.json(
            "https://api.test/repos/o/one/git/trees/commit-one",
            r#"{"sha":"root1","truncated":false,"tree":[
                {"path":"pdf","type":"tree","sha":"pdf-new"}]}"#,
        );
        let gh = client(http);
        let targets = vec![target("pdf", "o/one", "pdf", Some("pdf-old"))];
        let mut cache = RemoteCache::new();
        check_updates(&gh, &targets, &mut cache);
        check_updates(&gh, &targets, &mut cache);
        assert!(
            gh.http()
                .requests()
                .iter()
                .all(|request| request.headers.iter().all(|(n, _)| n != "If-None-Match"))
        );
    }

    // -- installing ----------------------------------------------------------

    /// The `SKILL.md` of the skill every install test downloads.
    const PDF_SKILL_MD: &str = "---\nname: pdf\ndescription: Reads PDFs.\n---\n\n# pdf\n";

    /// A repository holding one skill under `skills/pdf`.
    ///
    /// Scripted for both ways of getting the bytes, because an install takes
    /// whichever one it can: the recursive listing of `skills/pdf` and its two
    /// files from the raw host, and the whole repository archive that
    /// [`GitHub::fetch_files`] falls back to.
    fn install_http() -> FakeHttp {
        let http = FakeHttp::new();
        http.json(
            "https://api.test/repos/o/r/git/ref/heads/main",
            r#"{"object":{"sha":"c0ffee","type":"commit"}}"#,
        );
        http.json(
            "https://api.test/repos/o/r/git/trees/c0ffee",
            r#"{"sha":"root","truncated":false,"tree":[
                {"path":"skills","type":"tree","sha":"skills"}]}"#,
        );
        http.json(
            "https://api.test/repos/o/r/git/trees/skills",
            r#"{"sha":"skills","truncated":false,"tree":[
                {"path":"pdf","type":"tree","sha":"pdf-tree-sha"}]}"#,
        );
        http.json(
            "https://api.test/repos/o/r/git/trees/pdf-tree-sha?recursive=1",
            r#"{"sha":"pdf-tree-sha","truncated":false,"tree":[
                {"path":"SKILL.md","mode":"100644","type":"blob","sha":"skill-blob"},
                {"path":"scripts","mode":"040000","type":"tree","sha":"scripts-tree"},
                {"path":"scripts/run.sh","mode":"100755","type":"blob","sha":"run-blob"}]}"#,
        );
        // `skills` itself, for the test that asks to install a directory of
        // skills rather than a skill.
        http.json(
            "https://api.test/repos/o/r/git/trees/skills?recursive=1",
            r#"{"sha":"skills","truncated":false,"tree":[
                {"path":"pdf","mode":"040000","type":"tree","sha":"pdf-tree-sha"},
                {"path":"pdf/SKILL.md","mode":"100644","type":"blob","sha":"skill-blob"},
                {"path":"pdf/scripts","mode":"040000","type":"tree","sha":"scripts-tree"},
                {"path":"pdf/scripts/run.sh","mode":"100755","type":"blob","sha":"run-blob"},
                {"path":"other","mode":"040000","type":"tree","sha":"other-tree"},
                {"path":"other/SKILL.md","mode":"100644","type":"blob","sha":"other-blob"}]}"#,
        );
        http.reply(
            "https://raw.test/o/r/c0ffee/skills/pdf/SKILL.md",
            HttpResponse::new(200, PDF_SKILL_MD),
        );
        http.reply(
            "https://raw.test/o/r/c0ffee/skills/pdf/scripts/run.sh",
            HttpResponse::new(200, "#!/bin/sh\n"),
        );
        http.reply(
            "https://raw.test/o/r/c0ffee/skills/other/SKILL.md",
            HttpResponse::new(200, "---\nname: other\n---\n"),
        );
        http.reply(
            "https://codeload.test/o/r/tar.gz/c0ffee",
            HttpResponse::new(
                200,
                tarball(
                    "r-c0ffee",
                    &[
                        ("README.md", "not the skill\n"),
                        ("skills/pdf/SKILL.md", PDF_SKILL_MD),
                        ("skills/pdf/scripts/run.sh", "#!/bin/sh\n"),
                        ("skills/other/SKILL.md", "---\nname: other\n---\n"),
                    ],
                ),
            ),
        );
        http
    }

    /// Sets a cancel flag while the skill's files are being fetched.
    ///
    /// The only injection point an install has after the first cancel check:
    /// every later step is filesystem work with nothing to substitute. Without
    /// it a test can only ever trip the check that comes before the download.
    struct CancelOnDownload {
        inner: FakeHttp,
        cancel: Arc<AtomicBool>,
    }

    impl Http for CancelOnDownload {
        fn get(
            &self,
            url: &str,
            headers: &[(&str, &str)],
        ) -> Result<HttpResponse, crate::http::HttpError> {
            if url.contains("raw.test") || url.contains("codeload.test") {
                self.cancel.store(true, Ordering::SeqCst);
            }
            self.inner.get(url, headers)
        }
    }

    #[test]
    fn installing_writes_the_skill_and_its_provenance_into_the_store() {
        let fx = Fixture::empty();
        let gh = client(install_http());
        let mut cache = RemoteCache::new();
        let location = SkillLocation::parse("o/r/skills/pdf").unwrap();

        let installed = install_from_github(
            &fx.installer(),
            &gh,
            &location,
            &InstallOptions::new(),
            &mut cache,
        )
        .unwrap();

        assert_eq!(installed.name, "pdf");
        assert_eq!(installed.dir, fx.store().join("pdf"));
        assert_eq!(installed.files, 2);
        assert!(installed.dir.join("scripts/run.sh").is_file());
        assert!(!installed.dir.join("README.md").exists());

        let doc =
            SkillDoc::parse(&fs::read_to_string(installed.dir.join("SKILL.md")).unwrap()).unwrap();
        let provenance = Provenance::read(&doc.frontmatter).expect("provenance written");
        assert_eq!(provenance.repo_url, "https://github.com/o/r");
        assert_eq!(provenance.reference, "main");
        assert_eq!(provenance.tree_sha.as_deref(), Some("pdf-tree-sha"));
        assert_eq!(provenance.path, "skills/pdf");
        assert_eq!(doc.frontmatter.description(), Some("Reads PDFs."));

        // Recorded, and matching the directory as installed.
        assert_eq!(
            local_state(&cache, "pdf", &installed.dir),
            LocalState::Pristine
        );

        // Both shas, because they answer different questions. The tree sha is
        // what a later check compares; the commit sha is the only one of the
        // two GitHub's compare view will resolve.
        let record = cache.skill("pdf").expect("recorded");
        assert_eq!(record.tree_sha, "pdf-tree-sha");
        assert_eq!(record.commit_sha, "c0ffee");

        // Discovery sees it as a managed skill with no adoption step.
        let found = fx.scan();
        let skill = found.get("pdf").expect("discovered");
        assert!(skill.managed);

        // Nothing is left behind in staging.
        let staging = fx.home().join(STAGING_DIR);
        assert_eq!(fs::read_dir(&staging).unwrap().count(), 0);
    }

    #[test]
    fn installing_refuses_to_overwrite_unless_told_to() {
        let fx = Fixture::empty();
        fx.skill(".agents/skills/pdf", "pdf");
        let gh = client(install_http());
        let mut cache = RemoteCache::new();
        let location = SkillLocation::parse("o/r/skills/pdf").unwrap();

        let err = install_from_github(
            &fx.installer(),
            &gh,
            &location,
            &InstallOptions::new(),
            &mut cache,
        )
        .unwrap_err();
        assert!(
            matches!(err, FetchError::Install(InstallError::AlreadyExists { .. })),
            "{err:?}"
        );
        assert!(
            !fx.store().join("pdf/scripts").exists(),
            "the refused install changed nothing"
        );

        let installed = install_from_github(
            &fx.installer(),
            &gh,
            &location,
            &InstallOptions::new().replacing(),
            &mut cache,
        )
        .unwrap();
        assert!(installed.dir.join("scripts/run.sh").is_file());

        // The skill that was replaced went to the trash, not to nothing.
        let moved = installed
            .outcome
            .changes
            .iter()
            .find_map(|change| match change {
                Change::MovedToTrash { from, to } => Some((from.clone(), to.clone())),
                _ => None,
            })
            .expect("the old directory was moved to the trash");
        assert_eq!(moved.0, fx.store().join("pdf"));
        assert!(moved.1.starts_with(fx.roots().trash_dir()));
        assert!(
            fs::read_to_string(moved.1.join("SKILL.md"))
                .unwrap()
                .contains("name: pdf"),
            "the replaced skill is still readable under the trash"
        );
    }

    /// The half-done replace: the old skill is already in the trash and the new
    /// one cannot be put down. The staged directory is missing here, which is
    /// what a rename failing with ENOSPC or across a filesystem boundary
    /// amounts to from the destination's side — the old skill is gone from the
    /// store and only the error can say where it went.
    #[test]
    fn a_replace_that_cannot_land_the_new_skill_says_where_the_old_one_went() {
        let fx = Fixture::empty();
        let dest = fx.skill(".agents/skills/pdf", "pdf");
        let staged = fx.home().join(STAGING_DIR).join("pdf-never-extracted");

        let err = place_into_store(&fx.installer(), &staged, &dest, true).unwrap_err();

        let done = err.completed().expect("what was already done: {err:?}");
        let trashed = done
            .changes
            .iter()
            .find_map(|change| match change {
                Change::MovedToTrash { to, .. } => Some(to.clone()),
                _ => None,
            })
            .expect("the old directory was moved to the trash");
        assert!(!dest.exists(), "the store entry is gone");
        assert!(
            trashed.join("SKILL.md").is_file(),
            "and the skill is under the trash"
        );

        let message = err.to_string();
        assert!(
            message.contains(&trashed.display().to_string()),
            "the message names the directory the skill is in: {message}"
        );
        assert!(message.contains("Then it stopped: "), "{message}");

        // And the interface's rendering says the same with a `~`.
        let abbreviated = err.describe_under(fx.home());
        assert!(
            abbreviated.contains("to the trash at ~/.skillbase/trash/pdf-"),
            "{abbreviated}"
        );
        assert!(abbreviated.contains("Then it stopped: "), "{abbreviated}");
    }

    #[test]
    fn an_install_that_changed_nothing_before_it_failed_is_not_reported_as_partial() {
        let fx = Fixture::empty();
        let dest = fx.store().join("pdf");
        let staged = fx.home().join(STAGING_DIR).join("pdf-never-extracted");

        let err = place_into_store(&fx.installer(), &staged, &dest, true).unwrap_err();

        assert!(matches!(err, FetchError::Io { .. }), "{err:?}");
        assert!(err.completed().is_none());
    }

    /// What an install writes when the digest cannot be computed. The skill is
    /// already in the store by then, so the install is not failed; the record
    /// goes in without a digest, and the shas an update check needs are still
    /// there. An empty digest has to read as "no baseline", never as an edit,
    /// or a skill nobody touched would prompt on every update.
    #[test]
    fn a_record_with_no_digest_reads_as_unknown_rather_than_edited() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("pdf");
        write(&dir, "SKILL.md", "---\nname: pdf\n---\n");

        let mut cache = RemoteCache::new();
        cache.record_skill(
            "pdf",
            SkillRecord {
                digest: String::new(),
                tree_sha: "pdf-tree-sha".into(),
                commit_sha: "c0ffee".into(),
                recorded_at: now_unix(),
            },
        );

        assert_eq!(local_state(&cache, "pdf", &dir), LocalState::Unknown);
        let record = cache.skill("pdf").unwrap();
        assert_eq!(record.tree_sha, "pdf-tree-sha");
        assert_eq!(record.commit_sha, "c0ffee");
    }

    #[test]
    fn a_tree_that_cannot_be_renamed_across_a_filesystem_is_copied_instead() {
        let temp = tempfile::tempdir().unwrap();
        let from = temp.path().join("staged");
        write(&from, "SKILL.md", "---\nname: pdf\n---\n");
        write(&from, "scripts/run.sh", "#!/bin/sh\n");
        symlink("run.sh", from.join("scripts/also.sh")).unwrap();

        let to = temp.path().join("store/pdf");
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        copy_dir(&from, &to).unwrap();

        assert_eq!(
            fs::read_to_string(to.join("scripts/run.sh")).unwrap(),
            "#!/bin/sh\n"
        );
        assert_eq!(
            fs::read_link(to.join("scripts/also.sh")).unwrap(),
            Path::new("run.sh"),
            "a symlink is recreated as written, not followed"
        );
        assert_eq!(content_digest(&from).unwrap(), content_digest(&to).unwrap());
    }

    /// Cancelled before the call, so the check before the download is the one
    /// that stops it: nothing is downloaded and nothing is staged.
    #[test]
    fn a_cancelled_install_writes_nothing_and_stages_nothing() {
        let fx = Fixture::empty();
        let gh = client(install_http());
        let mut cache = RemoteCache::new();
        let cancel = Arc::new(AtomicBool::new(true));

        let err = install_from_github(
            &fx.installer(),
            &gh,
            &SkillLocation::parse("o/r/skills/pdf").unwrap(),
            &InstallOptions::new().cancelled_by(Arc::clone(&cancel)),
            &mut cache,
        )
        .unwrap_err();

        assert!(matches!(err, FetchError::Cancelled), "{err:?}");
        assert!(!fx.store().exists(), "nothing was written into the store");
        assert!(
            gh.http().urls().iter().all(|url| !url.contains("codeload")),
            "and the archive was never asked for: {:?}",
            gh.http().urls()
        );
        let staging = fx.home().join(STAGING_DIR);
        assert!(
            !staging.exists() || fs::read_dir(&staging).unwrap().count() == 0,
            "nothing was left in staging"
        );
        assert!(cache.skill("pdf").is_none(), "nothing was recorded");
    }

    /// Cancelled from inside the download, which is where a person cancelling a
    /// slow install actually gets to. Setting the flag before the call only ever
    /// trips the first check, which is before the archive is fetched and before
    /// a staging directory exists, so it can say nothing about the check that
    /// guards the destination.
    #[test]
    fn cancelling_during_the_download_still_replaces_nothing() {
        let fx = Fixture::empty();
        let existing = fx.skill(".agents/skills/pdf", "pdf");
        let cancel = Arc::new(AtomicBool::new(false));
        let gh = GitHub::new(CancelOnDownload {
            inner: install_http(),
            cancel: Arc::clone(&cancel),
        })
        .with_endpoints(
            "https://api.test",
            "https://codeload.test",
            "https://raw.test",
        );
        let mut cache = RemoteCache::new();
        let options = InstallOptions::new()
            .replacing()
            .cancelled_by(Arc::clone(&cancel));

        let err = install_from_github(
            &fx.installer(),
            &gh,
            &SkillLocation::parse("o/r/skills/pdf").unwrap(),
            &options,
            &mut cache,
        )
        .unwrap_err();

        assert!(matches!(err, FetchError::Cancelled), "{err:?}");
        assert!(existing.join("SKILL.md").is_file(), "untouched");
        assert!(!existing.join("scripts").exists(), "and not half-replaced");
        assert!(
            !fx.roots().trash_dir().exists(),
            "nothing was moved to the trash"
        );
        // The archive really was downloaded and extracted, so a staging
        // directory really did exist and `Staging::drop` really did take it
        // away. Without the download this assertion would hold vacuously.
        let staging = fx.home().join(STAGING_DIR);
        assert!(staging.is_dir(), "the install got as far as staging");
        assert_eq!(fs::read_dir(&staging).unwrap().count(), 0);
        assert!(cache.skill("pdf").is_none(), "nothing was recorded");
    }

    #[test]
    fn a_directory_without_a_skill_file_is_refused_and_leaves_nothing_behind() {
        let fx = Fixture::empty();
        let gh = client(install_http());
        let mut cache = RemoteCache::new();
        let location = SkillLocation::parse("o/r/skills").unwrap();
        // `skills` itself is a directory of skills, not a skill.
        let err = install_from_github(
            &fx.installer(),
            &gh,
            &location,
            &InstallOptions::new(),
            &mut cache,
        )
        .unwrap_err();
        assert!(matches!(err, FetchError::NotASkill { .. }), "{err:?}");
        assert!(!fx.store().join("skills").exists());
        assert_eq!(
            fs::read_dir(fx.home().join(STAGING_DIR)).unwrap().count(),
            0
        );
    }

    /// Issue 12: `pdftk-server` sits in `github/awesome-copilot`, whose archive
    /// is 86 MB, and installing it failed on the transport's size cap. The
    /// skill's own directory is a few files, and that is all this now asks for,
    /// so the archive's size stops mattering.
    #[test]
    fn a_skill_installs_out_of_a_repository_too_large_to_download_whole() {
        let fx = Fixture::empty();
        let http = install_http();
        http.too_large("https://codeload.test/o/r/tar.gz/c0ffee");
        let gh = client(http);
        let mut cache = RemoteCache::new();

        let installed = install_from_github(
            &fx.installer(),
            &gh,
            &SkillLocation::parse("o/r/skills/pdf").unwrap(),
            &InstallOptions::new(),
            &mut cache,
        )
        .expect("the skill's own directory is small whatever the repository weighs");

        assert_eq!(installed.name, "pdf");
        assert_eq!(installed.files, 2);
        assert!(installed.dir.join("scripts/run.sh").is_file());
    }

    /// The other half of issue 12: installing the whole repository has no
    /// smaller request to fall back on, so the failure has to be a sentence the
    /// user can act on rather than a URL and a byte count.
    #[test]
    fn installing_a_whole_repository_too_large_to_download_says_what_to_do_instead() {
        let fx = Fixture::empty();
        let http = install_http();
        http.too_large("https://codeload.test/o/r/tar.gz/c0ffee");
        let gh = client(http);
        let mut cache = RemoteCache::new();

        let err = install_from_github(
            &fx.installer(),
            &gh,
            &SkillLocation::parse("o/r").unwrap(),
            &InstallOptions::new(),
            &mut cache,
        )
        .unwrap_err();

        assert_eq!(
            err.describe_under(fx.home()),
            "o/r is too large to download whole (over 64 MB). Install one skill from it \
             instead of the whole repository, by naming that skill's directory: \
             o/r/path/to/skill."
        );
        assert_eq!(
            fs::read_dir(fx.home().join(STAGING_DIR)).unwrap().count(),
            0,
            "and it leaves no staging directory behind"
        );
    }

    #[test]
    fn an_installed_skill_is_seen_as_pristine_and_then_as_edited() {
        let fx = Fixture::empty();
        let gh = client(install_http());
        let mut cache = RemoteCache::new();
        let installed = install_from_github(
            &fx.installer(),
            &gh,
            &SkillLocation::parse("o/r/skills/pdf").unwrap(),
            &InstallOptions::new(),
            &mut cache,
        )
        .unwrap();

        fs::write(installed.dir.join("scripts/run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        assert_eq!(
            local_state(&cache, "pdf", &installed.dir),
            LocalState::Edited
        );
    }

    #[test]
    fn a_lost_cache_still_answers_by_downloading_upstream() {
        let fx = Fixture::empty();
        let gh = client(install_http());
        let mut cache = RemoteCache::new();
        let location = SkillLocation::parse("o/r/skills/pdf").unwrap();
        let installed = install_from_github(
            &fx.installer(),
            &gh,
            &location,
            &InstallOptions::new(),
            &mut cache,
        )
        .unwrap();

        // The cache is gone. The recorded digest went with it.
        let lost = RemoteCache::new();
        assert_eq!(
            local_state(&lost, "pdf", &installed.dir),
            LocalState::Unknown
        );
        assert_eq!(
            upstream_state(&gh, &fx.roots(), &location, &installed.dir).unwrap(),
            LocalState::Pristine
        );

        fs::write(
            installed.dir.join("SKILL.md"),
            "---\nname: pdf\n---\nedited\n",
        )
        .unwrap();
        assert_eq!(
            upstream_state(&gh, &fx.roots(), &location, &installed.dir).unwrap(),
            LocalState::Edited
        );
    }

    // -- staging -------------------------------------------------------------

    #[test]
    fn a_sweep_of_a_home_that_never_staged_anything_has_nothing_to_say() {
        let fx = Fixture::empty();
        let sweep = sweep_staging(&fx.roots());

        assert!(sweep.is_clean());
        assert!(sweep.removed().is_empty());
        assert!(sweep.warning().is_none());
    }

    #[test]
    fn a_sweep_leaves_alone_what_a_running_install_might_own() {
        let fx = Fixture::empty();
        let live = fx.dir(&format!("{STAGING_DIR}/pdf-1-1"));

        // The real threshold, against a directory made a moment ago: another
        // copy of Skillbase could be extracting into it right now.
        let sweep = sweep_staging(&fx.roots());

        assert!(sweep.removed().is_empty(), "{sweep:?}");
        assert!(sweep.is_clean());
        assert!(live.is_dir());
    }

    #[test]
    fn a_sweep_takes_away_what_a_killed_install_left_behind() {
        let fx = Fixture::empty();
        let root = fx.dir(STAGING_DIR);
        let abandoned = fx.dir(&format!("{STAGING_DIR}/pdf-1-1"));
        fs::write(abandoned.join("SKILL.md"), "half a skill\n").unwrap();
        fs::write(abandoned.join("big.bin"), "x".repeat(4096)).unwrap();
        let stray = fx.write_file(&format!("{STAGING_DIR}/stray.tmp"), "x");

        // Age zero, so everything counts as abandoned without waiting an hour.
        let sweep = sweep_staging_dir(&root, Duration::ZERO);

        assert_eq!(sweep.removed(), [abandoned.clone(), stray.clone()]);
        assert!(sweep.is_clean());
        assert!(sweep.warning().is_none());
        assert!(!abandoned.exists());
        assert!(!stray.exists());
    }

    #[test]
    fn a_sweep_that_cannot_remove_names_the_directory_it_left() {
        use std::os::unix::fs::PermissionsExt;

        let fx = Fixture::empty();
        let root = fx.dir(STAGING_DIR);
        let stuck = fx.dir(&format!("{STAGING_DIR}/pdf-1-1"));
        // Readable, so the sweep sees it; not writable, so it cannot act.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o555)).unwrap();

        let sweep = sweep_staging_dir(&root, Duration::ZERO);

        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(!sweep.is_clean(), "{sweep:?}");
        assert_eq!(sweep.failed().len(), 1, "{sweep:?}");
        assert_eq!(sweep.failed()[0].0, stuck);
        let warning = sweep.warning().expect("a sentence naming the directory");
        assert!(warning.contains("pdf-1-1"), "{warning}");
        assert!(stuck.is_dir(), "and the space really is still spent");
    }

    /// Backdates `path` so a sweep counts it as abandoned. An install sweeps
    /// with [`STALE_STAGING_AGE`] rather than an age a test can pass in, so the
    /// only way to plant something an install will take away is to make it old.
    fn backdate(path: &Path, by: Duration) {
        let times = fs::FileTimes::new().set_modified(SystemTime::now() - by);
        fs::File::open(path).unwrap().set_times(times).unwrap();
    }

    #[test]
    fn an_install_sweeps_staging_and_reports_what_the_sweep_found() {
        let fx = Fixture::empty();
        // What a run that was killed mid-install left behind, old enough that
        // no install still running could own it.
        let abandoned = fx.dir(&format!("{STAGING_DIR}/pdf-1-1"));
        fs::write(abandoned.join("SKILL.md"), "half a skill\n").unwrap();
        backdate(&abandoned, STALE_STAGING_AGE * 2);

        let gh = client(install_http());
        let mut cache = RemoteCache::new();
        let installed = install_from_github(
            &fx.installer(),
            &gh,
            &SkillLocation::parse("o/r/skills/pdf").unwrap(),
            &InstallOptions::new(),
            &mut cache,
        )
        .unwrap();

        assert!(installed.staging.is_clean(), "{:?}", installed.staging);
        assert!(installed.staging.warning().is_none());
        // The wiring this test is named for: what `Staging::new` swept reaches
        // `Installed::staging` through `Staging::keep`.
        assert_eq!(
            installed.staging.removed(),
            std::slice::from_ref(&abandoned)
        );
        assert!(!abandoned.exists());
        // And this install's own staging directory went with `Staging::drop`,
        // not with the sweep.
        let staging = fx.home().join(STAGING_DIR);
        assert_eq!(fs::read_dir(&staging).unwrap().count(), 0);
    }
}
