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
//! # The cache is disposable
//!
//! [`REMOTE_CACHE_FILE`] is modelled on the usage cache: a version, and
//! missing, corrupt or differently versioned all mean "work it out again"
//! rather than an error. Losing it costs speed, never correctness:
//! [`upstream_state`] answers the same question by downloading upstream and
//! comparing, and update checking still works because the baseline sha lives in
//! the `SKILL.md`, not in the cache.
//!
//! Every call here blocks. Run them on a background task.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::discovery::DiscoveredSkill;
use crate::doc::SkillDoc;
use crate::error::SkillError;
use crate::github::{GitHub, GitHubError, RateLimit, RepoRef, SkillLocation, extract_subdir};
use crate::http::Http;
use crate::install::{Change, InstallError, Installer, Outcome};
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

    /// Writes the cache under `roots`, ignoring every failure.
    pub fn write(&self, roots: &Roots) {
        self.write_file(&Self::path(roots));
    }

    /// Writes the cache to an explicit path, ignoring every failure.
    ///
    /// Written to a sibling and renamed, so an interrupted write leaves the
    /// previous cache rather than a truncated one.
    pub fn write_file(&self, path: &Path) {
        let mut cache = self.clone();
        cache.version = CACHE_VERSION;
        let Ok(text) = serde_json::to_string(&cache) else {
            return;
        };
        if let Some(parent) = path.parent()
            && fs::create_dir_all(parent).is_err()
        {
            return;
        }
        let temp = path.with_extension("json.tmp");
        if fs::write(&temp, text).is_ok() && fs::rename(&temp, path).is_err() {
            let _ = fs::remove_file(&temp);
        }
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

/// The key a repository and ref are recorded under: `owner/repo@ref`.
pub fn repo_key(repo: &RepoRef) -> String {
    format!("{}@{}", repo.slug(), repo.reference)
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
    let archive = gh.download_tarball(&location.repo, &commit)?;
    extract_subdir(&archive, &location.path, staged.path())?;

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

    /// A filesystem call failed.
    #[error("{path}: {source}")]
    Io {
        /// The path being read or written.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
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
}

/// How to install.
#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    /// Install under this name instead of the one in the frontmatter.
    pub name: Option<String>,
    /// Replace a skill of the same name already in the store. Off by default,
    /// so installing never overwrites without being told to.
    pub replace: bool,
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
    pub digest: String,
    /// How many files were extracted.
    pub files: usize,
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
/// 2. Download the archive pinned to that commit sha, and extract only the
///    wanted subdirectory into [`STAGING_DIR`].
/// 3. Write the provenance into the staged `SKILL.md`, under `metadata`.
/// 4. Move the staged directory into the store. Nothing appears in
///    `~/.agents/skills` until the skill is complete.
/// 5. Record the content digest, so a later check can tell an edited skill from
///    an untouched one.
///
/// The staging directory is removed whether this succeeds or fails.
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
    let archive = gh.download_tarball(&location.repo, &commit)?;

    let staged = Staging::new(roots, location.dir_name())?;
    let files = extract_subdir(&archive, &location.path, staged.path())?;
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

    let dest = installer.ensure_in_scope(&roots.store_dir().join(&name))?;
    let mut outcome = Outcome::default();
    if fs::symlink_metadata(&dest).is_ok() {
        if !options.replace {
            return Err(InstallError::AlreadyExists { path: dest }.into());
        }
        outcome
            .changes
            .extend(replace_existing(installer, &dest)?.changes);
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| FetchError::io(parent, e))?;
    }
    fs::rename(staged.path(), &dest).map_err(|e| FetchError::io(&dest, e))?;
    staged.keep();

    let digest = content_digest(&dest).map_err(|e| FetchError::io(&dest, e))?;
    cache.record_skill(
        &name,
        SkillRecord {
            digest: digest.clone(),
            tree_sha: tree_sha.clone(),
            recorded_at: now_unix(),
        },
    );

    outcome
        .changes
        .push(Change::CreatedDirectory { path: dest.clone() });
    outcome.changes.push(Change::WroteFile {
        path: dest.join(SKILL_FILE_NAME),
    });

    Ok(Installed {
        name,
        dir: dest,
        provenance,
        digest,
        files,
        outcome,
    })
}

/// Removes what is already at `dest` before an install replaces it.
///
/// Goes through [`Installer::ensure_in_scope`] first, and refuses to follow a
/// symlink: a link at that path is unlinked, and only a real directory is
/// removed.
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
    if !meta.is_dir() {
        return Err(InstallError::NotALink {
            path: dest,
            kind: "file",
        });
    }
    fs::remove_dir_all(&dest).map_err(|e| InstallError::Io {
        path: dest.clone(),
        source: e,
    })?;
    Ok(Outcome::one(Change::RemovedDirectory { path: dest }))
}

/// Writes `provenance` into the `SKILL.md` in `dir` and returns the skill's
/// `name` from the frontmatter, when it has one.
///
/// The rest of the file is untouched: the body is never re-rendered, and every
/// other frontmatter key keeps its value and its position.
fn write_provenance(dir: &Path, provenance: &Provenance) -> Result<Option<String>, FetchError> {
    let path = dir.join(SKILL_FILE_NAME);
    let source = fs::read_to_string(&path).map_err(|e| FetchError::io(&path, e))?;
    let mut doc = SkillDoc::parse(&source)?;
    provenance.write(&mut doc.frontmatter);
    fs::write(&path, doc.to_markdown()).map_err(|e| FetchError::io(&path, e))?;
    Ok(doc.frontmatter.name().map(str::to_string))
}

/// A directory under [`STAGING_DIR`], removed when it goes out of scope unless
/// it was kept.
///
/// A download is assembled here rather than in the store, so an interrupted
/// install leaves nothing an agent could load.
struct Staging {
    path: PathBuf,
    keep: bool,
}

impl Staging {
    /// Creates a staging directory whose name will not collide with a
    /// concurrent install.
    fn new(roots: &Roots, label: &str) -> Result<Staging, FetchError> {
        let root = roots.home().join(STAGING_DIR);
        fs::create_dir_all(&root).map_err(|e| FetchError::io(&root, e))?;
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let label = slugify(label);
        let path = root.join(format!("{label}-{}-{unique}", std::process::id()));
        // A leftover from a crashed run under the same name is not reused.
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).map_err(|e| FetchError::io(&path, e))?;
        Ok(Staging { path, keep: false })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// The directory has been moved away, so there is nothing left to remove.
    fn keep(mut self) {
        self.keep = true;
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.keep {
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
                recorded_at: 7,
            },
        );
        cache.write(&fx.roots());
        let read = RemoteCache::read(&fx.roots());
        assert_eq!(read.skill("pdf").unwrap().digest, "abc");

        fx.write_file(REMOTE_CACHE_FILE, "{ not json ");
        assert!(RemoteCache::read(&fx.roots()).is_empty());

        fx.write_file(REMOTE_CACHE_FILE, r#"{"version":99,"skills":{"pdf":{}}}"#);
        assert!(RemoteCache::read(&fx.roots()).is_empty());

        assert!(RemoteCache::read_file(Path::new("/nonexistent/remote.json")).is_empty());
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
                recorded_at: 0,
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
        GitHub::new(http).with_endpoints("https://api.test", "https://codeload.test")
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
                recorded_at: 0,
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

    /// A repository holding one skill under `skills/pdf`.
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
        http.reply(
            "https://codeload.test/o/r/tar.gz/c0ffee",
            HttpResponse::new(
                200,
                tarball(
                    "r-c0ffee",
                    &[
                        ("README.md", "not the skill\n"),
                        (
                            "skills/pdf/SKILL.md",
                            "---\nname: pdf\ndescription: Reads PDFs.\n---\n\n# pdf\n",
                        ),
                        ("skills/pdf/scripts/run.sh", "#!/bin/sh\n"),
                        ("skills/other/SKILL.md", "---\nname: other\n---\n"),
                    ],
                ),
            ),
        );
        http
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
}
