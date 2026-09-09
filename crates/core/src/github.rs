//! Reading skills out of GitHub repositories.
//!
//! Three questions, each answered with the cheapest request that answers it:
//!
//! 1. **Did the repository move?** `GET /repos/{owner}/{repo}/git/ref/heads/
//!    {branch}` returns about 371 bytes and one commit sha. Checking updates
//!    starts here, batched by repository, because thirty-five installed skills
//!    on a real machine came from thirteen repositories: thirteen requests, not
//!    thirty-five.
//! 2. **Did this skill's directory move?** Walk trees. `GET /repos/{o}/{r}/git/
//!    trees/{sha}` lists one level, each entry carrying `{path, mode, type,
//!    sha}`; the `tree` entry whose path matches the next segment gives the sha
//!    to fetch next. Two or three of those cost about 3.5 KB against the
//!    roughly 160 KB `?recursive=1` returns for the same repository. A
//!    truncated response falls back to the recursive form, which is the only
//!    case where it is worth paying for.
//! 3. **Give me the bytes.** [`GitHub::fetch_files`] lists the skill's own
//!    directory with one recursive tree request and then pulls each file from
//!    `https://raw.githubusercontent.com/{o}/{r}/{sha}/{path}`, which is not
//!    counted against the API rate limit. Installing `skills/pdftk-server` out
//!    of `github/awesome-copilot` moves about 29 KB that way; the repository's
//!    own archive is 86 MB, which is past the transport's
//!    [`MAX_BODY_BYTES`](crate::http::MAX_BODY_BYTES) and so could not be
//!    downloaded at all. The whole archive,
//!    `https://codeload.github.com/{o}/{r}/tar.gz/{sha}` unpacked by
//!    [`extract_subdir`], is the fallback for the cases the file-by-file fetch
//!    cannot serve — see [`WholeRepoReason`].
//!
//! The contents API is never used to detect change. It does not recurse, so an
//! edit inside `scripts/` does not alter anything it reports.
//!
//! # Rate limits and conditional requests
//!
//! GitHub allows 60 requests an hour per IP unauthenticated and 5000 an hour
//! with a token. [`GitHub::from_env`] takes [`GITHUB_TOKEN_ENV`] when it is
//! set, otherwise the token from `gh auth token`, otherwise none. Every API
//! response carries `x-ratelimit-remaining` and `x-ratelimit-reset`, which
//! [`GitHub::rate_limit`] reports so the interface can say "rate limited
//! until 14:30" instead of failing blankly.
//!
//! `If-None-Match` is sent only when a token is present. A 304 from a
//! conditional request does not consume quota for an authenticated caller, but
//! it *does* consume quota for an unauthenticated one, so sending an ETag
//! without a token would spend the 60-request budget to be told nothing
//! changed.
//!
//! Every call blocks. Run them on a background task.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use thiserror::Error;

use crate::http::{Http, HttpError, HttpResponse};
use crate::provenance::{DEFAULT_BRANCH, Provenance, split_owner_repo};

/// Environment variable holding an optional GitHub token.
///
/// Any token GitHub accepts works; a fine-grained token needs only public
/// repository read access. Without one the limit is 60 requests an hour per IP.
pub const GITHUB_TOKEN_ENV: &str = "SKILLBASE_GITHUB_TOKEN";

/// The GitHub REST API.
pub const GITHUB_API_BASE: &str = "https://api.github.com";

/// The host that serves repository archives. Not on the API rate limit.
pub const GITHUB_CODELOAD_BASE: &str = "https://codeload.github.com";

/// The host that serves single files out of a repository, pinned to a sha.
/// Not on the API rate limit either; it sends no `x-ratelimit-*` header at all.
pub const GITHUB_RAW_BASE: &str = "https://raw.githubusercontent.com";

/// The REST API version this crate asks for.
const API_VERSION: &str = "2022-11-28";

/// How many files [`GitHub::fetch_files`] will fetch one at a time before it
/// downloads the whole repository archive instead.
///
/// A skill is a `SKILL.md` and a handful of references and scripts; the largest
/// in the wild are a few dozen files. Past this many, one archive request beats
/// a hundred file requests, and a directory that big is not a skill.
pub const MAX_SUBTREE_FILES: usize = 100;

/// What the rate limit headers said on the last API response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    /// Requests left in the current window.
    pub remaining: u32,
    /// Requests allowed per window: 60 unauthenticated, 5000 with a token.
    pub limit: u32,
    /// When the window resets, in seconds since the Unix epoch.
    pub reset_unix: i64,
}

impl RateLimit {
    /// True when no requests are left.
    pub fn is_exhausted(&self) -> bool {
        self.remaining == 0
    }

    /// When the window resets, as a [`SystemTime`], for formatting a local
    /// time in the interface.
    pub fn reset_at(&self) -> Option<SystemTime> {
        let seconds = u64::try_from(self.reset_unix).ok()?;
        UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
    }

    /// How long until the window resets, measured from `now`. Zero once the
    /// reset time has passed.
    pub fn wait_from(&self, now: SystemTime) -> Duration {
        match self.reset_at() {
            Some(reset) => reset.duration_since(now).unwrap_or(Duration::ZERO),
            None => Duration::ZERO,
        }
    }

    /// Reads the three headers from a response. `None` when none are present,
    /// which is how a non-API host answers.
    fn from_response(response: &HttpResponse) -> Option<RateLimit> {
        let remaining = response.header_int("x-ratelimit-remaining")?;
        Some(RateLimit {
            remaining: u32::try_from(remaining).unwrap_or(0),
            limit: u32::try_from(response.header_int("x-ratelimit-limit").unwrap_or(0))
                .unwrap_or(0),
            reset_unix: response.header_int("x-ratelimit-reset").unwrap_or(0),
        })
    }
}

/// Anything that can go wrong talking to GitHub.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GitHubError {
    /// The request never reached GitHub.
    #[error(transparent)]
    Http(#[from] HttpError),

    /// The rate limit is spent. Carries the reset time, so the interface can
    /// name it.
    #[error("GitHub rate limit reached ({limit} requests per hour); it resets at {reset}")]
    RateLimited {
        /// Requests allowed per window.
        limit: u32,
        /// The reset time, in seconds since the Unix epoch.
        reset: i64,
    },

    /// GitHub has no such repository, ref or object, or it is private and this
    /// caller has no token for it.
    #[error("{what} not found on GitHub")]
    NotFound {
        /// What was being looked for, in words.
        what: String,
    },

    /// GitHub answered, with a status this crate did not expect.
    #[error("GitHub answered {status} for {url}: {message}")]
    Status {
        /// The URL requested.
        url: String,
        /// The status code.
        status: u16,
        /// GitHub's own message, when it sent one.
        message: String,
    },

    /// The response parsed as JSON but not as the shape expected.
    #[error("unexpected response from {url}: {detail}")]
    Malformed {
        /// The URL requested.
        url: String,
        /// What was wrong with it.
        detail: String,
    },

    /// The repository holds no directory at that path.
    #[error("{repo} has no directory `{path}` at {reference}")]
    PathNotFound {
        /// The repository, as `owner/repo`.
        repo: String,
        /// The path that was looked for.
        path: String,
        /// The ref or sha it was looked for at.
        reference: String,
    },

    /// The downloaded archive could not be read.
    #[error("could not read the downloaded archive: {detail}")]
    Archive {
        /// What went wrong.
        detail: String,
    },

    /// The repository's archive is bigger than
    /// [`MAX_BODY_BYTES`](crate::http::MAX_BODY_BYTES), and the wanted files
    /// could not be fetched one at a time either.
    ///
    /// [`GitHub::fetch_files`] reaches for the archive only when
    /// [`WholeRepoReason`] says the file-by-file fetch cannot serve the
    /// request, so this error always carries why it had to.
    #[error("{}", too_large_message(.repo, .path, .limit, .reason))]
    RepoTooLarge {
        /// The repository, as `owner/repo`.
        repo: String,
        /// The directory that was wanted, empty for the repository root.
        path: String,
        /// The cap that was passed,
        /// [`MAX_BODY_BYTES`](crate::http::MAX_BODY_BYTES).
        limit: usize,
        /// Why the whole archive was the only way to get the files.
        reason: WholeRepoReason,
    },

    /// Writing an extracted file failed.
    #[error("{path}: {source}")]
    Io {
        /// The path being written.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

impl GitHubError {
    /// Builds a [`GitHubError::Io`] carrying the path that failed.
    fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// True when this error will keep happening until the rate limit resets, so
    /// a caller checking many repositories should stop rather than spend the
    /// rest of its list on the same answer.
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, Self::RateLimited { .. })
    }
}

/// Why [`GitHub::fetch_files`] downloaded the whole repository archive rather
/// than the wanted directory alone.
///
/// Only interesting when the archive then turned out to be too large: it is
/// what lets [`GitHubError::RepoTooLarge`] say whether the user asked for the
/// whole repository, in which case naming one skill inside it works, or asked
/// for a directory that could not be fetched by itself, in which case it does
/// not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WholeRepoReason {
    /// The repository root was asked for, so the whole repository *is* the
    /// skill and there is no smaller thing to fetch.
    WholeRepository,
    /// The directory holds a symlink. Only [`extract_subdir`] recreates one,
    /// and only it checks where the link points.
    HasSymlink,
    /// The directory holds more than [`MAX_SUBTREE_FILES`] files.
    TooManyFiles,
    /// GitHub could not list the directory in one response, so an entry that
    /// is absent from the listing proves nothing.
    Truncated,
}

impl WholeRepoReason {
    /// The clause that finishes "…could not be fetched on its own because …".
    fn because(self) -> &'static str {
        match self {
            // Never reached: the whole-repository case takes the other branch
            // of `too_large_message`.
            Self::WholeRepository => "it is the whole repository",
            Self::HasSymlink => "it holds a symlink",
            Self::TooManyFiles => "it holds more files than can be fetched one at a time",
            Self::Truncated => "GitHub could not list it in one response",
        }
    }
}

/// What [`GitHubError::RepoTooLarge`] says.
///
/// Two sentences, and the second is the one that matters: it is the difference
/// between a user who tries the next thing and one who concludes the skill is
/// broken. Asking for the whole repository has a real next step — name the one
/// skill wanted — so the message names it, spelled the way the field takes it.
/// The other cases have no shorter download to offer and say so.
fn too_large_message(repo: &str, path: &str, limit: &usize, reason: &WholeRepoReason) -> String {
    let megabytes = limit / (1024 * 1024);
    match reason {
        WholeRepoReason::WholeRepository => format!(
            "{repo} is too large to download whole (over {megabytes} MB). Install one skill \
             from it instead of the whole repository, by naming that skill's directory: \
             {repo}/path/to/skill."
        ),
        other => format!(
            "{repo} is too large to download whole (over {megabytes} MB), and `{path}` could \
             not be fetched on its own because {}. Clone the repository and install the skill \
             from that folder instead.",
            other.because()
        ),
    }
}

/// A repository and the branch or tag to read it at.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoRef {
    /// The account or organisation.
    pub owner: String,
    /// The repository name.
    pub repo: String,
    /// A branch name, a tag name, or a full 40-character commit sha.
    pub reference: String,
}

impl RepoRef {
    /// A repository at an explicit ref.
    pub fn new(
        owner: impl Into<String>,
        repo: impl Into<String>,
        reference: impl Into<String>,
    ) -> Self {
        Self {
            owner: owner.into(),
            repo: repo.into(),
            reference: reference.into(),
        }
    }

    /// The repository's web URL, which is what provenance records.
    pub fn url(&self) -> String {
        format!("https://github.com/{}/{}", self.owner, self.repo)
    }

    /// `owner/repo`.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// True when [`RepoRef::reference`] is already a full commit sha, so no
    /// request is needed to resolve it.
    pub fn is_sha(&self) -> bool {
        is_full_sha(&self.reference)
    }
}

/// True for a 40-character lowercase hex sha.
fn is_full_sha(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A skill's location: a repository, a ref, and the subdirectory holding its
/// `SKILL.md`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SkillLocation {
    /// The repository and ref.
    pub repo: RepoRef,
    /// The subdirectory, with no leading or trailing slash. Empty at the
    /// repository root.
    pub path: String,
}

/// A spec read into a location, and whether the spec named the ref itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLocation {
    /// Where the skill is. When `explicit_ref` is false the location's ref is
    /// [`DEFAULT_BRANCH`], which is a guess the caller should replace with the
    /// repository's real default branch before installing.
    pub location: SkillLocation,
    /// True when the spec carried its own ref, as `owner/repo@ref` or as the
    /// `<ref>` of a `/tree/<ref>/...` URL.
    pub explicit_ref: bool,
}

impl SkillLocation {
    /// A location from its parts. The path is trimmed of slashes.
    pub fn new(repo: RepoRef, path: impl AsRef<str>) -> Self {
        Self {
            repo,
            path: path.as_ref().trim_matches('/').to_string(),
        }
    }

    /// Reads the spellings a user is likely to paste or type. See
    /// [`SkillLocation::parse_spec`] for the list.
    ///
    /// The ref defaults to [`DEFAULT_BRANCH`] when the spelling does not carry
    /// one, and this form cannot tell that default from a ref the user typed.
    /// A caller that has to know — because a repository whose default branch is
    /// `master` would otherwise be looked up at a `main` nobody asked for —
    /// should use [`SkillLocation::parse_spec`] instead.
    ///
    /// Returns `None` when no `owner` and `repo` can be read.
    pub fn parse(spec: &str) -> Option<SkillLocation> {
        Self::parse_spec(spec).map(|parsed| parsed.location)
    }

    /// Reads the spellings a user is likely to paste or type:
    ///
    /// * `owner/repo`
    /// * `owner/repo@ref`
    /// * `owner/repo/sub/dir`, and `owner/repo/sub/dir@ref`
    /// * `https://github.com/owner/repo`
    /// * `https://github.com/owner/repo/tree/<ref>/<sub/dir>`
    /// * `git@github.com:owner/repo.git`
    ///
    /// The ref defaults to [`DEFAULT_BRANCH`] when the spelling does not carry
    /// one, and [`ParsedLocation::explicit_ref`] reports which of the two
    /// happened. Returns `None` when no `owner` and `repo` can be read.
    pub fn parse_spec(spec: &str) -> Option<ParsedLocation> {
        let spec = spec.trim().trim_end_matches('/');
        // A trailing `@ref`, but not the `@` in `git@github.com:owner/repo`.
        // The difference is position: a ref suffix comes after the last slash.
        let (spec, at_ref) = match spec.rfind('@') {
            Some(index) if index > spec.rfind('/').unwrap_or(0) => {
                (&spec[..index], Some(spec[index + 1..].to_string()))
            }
            _ => (spec, None),
        };

        let (owner, repo) = split_owner_repo(spec)?;

        // Everything after `owner/repo`, whatever the prefix was.
        let tail = spec
            .split_once(&format!("{owner}/{repo}"))
            .map(|(_, tail)| tail.trim_matches('/'))
            .unwrap_or("")
            .trim_end_matches(".git")
            .trim_matches('/');

        let mut explicit_ref = at_ref.is_some();
        let (reference, path) = if let Some(rest) = tail.strip_prefix("tree/") {
            let (reference, path) = match rest.split_once('/') {
                Some((reference, path)) => (reference.to_string(), path.to_string()),
                None => (rest.to_string(), String::new()),
            };
            // The `<ref>` of a `/tree/<ref>/...` URL was typed just as
            // deliberately as an `@ref` suffix.
            explicit_ref = explicit_ref || !reference.is_empty();
            (reference, path)
        } else {
            (
                at_ref.clone().unwrap_or_else(|| DEFAULT_BRANCH.to_string()),
                tail.to_string(),
            )
        };
        let reference = at_ref.unwrap_or(reference);

        Some(ParsedLocation {
            location: SkillLocation::new(RepoRef::new(owner, repo, reference), path),
            explicit_ref,
        })
    }

    /// The last path segment, which is the directory name the skill is
    /// installed under. The repository name when the skill sits at the root.
    pub fn dir_name(&self) -> &str {
        self.path
            .rsplit('/')
            .find(|segment| !segment.is_empty())
            .unwrap_or(&self.repo.repo)
    }

    /// The provenance to write into the installed `SKILL.md`, given the tree
    /// sha resolved for this location.
    pub fn provenance(&self, tree_sha: impl Into<String>) -> Provenance {
        Provenance::new(self.repo.url(), &self.repo.reference, tree_sha, &self.path)
    }
}

/// What a ref currently points at, and the ETag to ask with next time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefState {
    /// The commit sha, or `None` when GitHub answered 304 Not Modified and the
    /// sha the caller already had is still current.
    pub sha: Option<String>,
    /// The ETag of the response, to send as `If-None-Match` next time.
    pub etag: Option<String>,
}

/// One entry in a git tree.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TreeEntry {
    /// The entry's path, relative to the tree it was listed in — or to the
    /// repository root when the tree was fetched recursively.
    pub path: String,
    /// The file mode, e.g. `100644` or `040000`.
    #[serde(default)]
    pub mode: String,
    /// `blob`, `tree` or `commit`. A `commit` entry is a submodule.
    #[serde(rename = "type", default)]
    pub kind: String,
    /// The entry's own sha.
    #[serde(default)]
    pub sha: String,
}

impl TreeEntry {
    /// True for a directory entry.
    pub fn is_tree(&self) -> bool {
        self.kind == "tree"
    }

    /// True for a file entry.
    pub fn is_blob(&self) -> bool {
        self.kind == "blob"
    }

    /// True for a symlink, which git records as a blob with mode `120000`
    /// whose content is the target path.
    pub fn is_symlink(&self) -> bool {
        self.mode == "120000"
    }
}

/// One git tree, as GitHub returns it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Tree {
    /// The tree's own sha.
    #[serde(default)]
    pub sha: String,
    /// Its entries.
    #[serde(default)]
    pub tree: Vec<TreeEntry>,
    /// True when GitHub could not fit the whole tree in one response. Anything
    /// read from a truncated tree may be missing, so callers refetch.
    #[serde(default)]
    pub truncated: bool,
}

impl Tree {
    /// The entry whose path is exactly `path`.
    pub fn entry(&self, path: &str) -> Option<&TreeEntry> {
        self.tree.iter().find(|entry| entry.path == path)
    }
}

/// A GitHub client over any [`Http`].
///
/// Endpoints are fields rather than constants so tests point the client at a
/// fake and never open a socket.
pub struct GitHub<H: Http> {
    http: H,
    token: Option<String>,
    api_base: String,
    codeload_base: String,
    raw_base: String,
    rate_limit: Mutex<Option<RateLimit>>,
    requests: AtomicUsize,
}

impl<H: Http + fmt::Debug> fmt::Debug for GitHub<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHub")
            .field("http", &self.http)
            .field("token", &self.token.as_ref().map(|_| "set"))
            .field("api_base", &self.api_base)
            .field("codeload_base", &self.codeload_base)
            .field("raw_base", &self.raw_base)
            .field("rate_limit", &self.rate_limit)
            .field("requests", &self.requests)
            .finish()
    }
}

impl<H: Http> GitHub<H> {
    /// A client with no token: 60 requests an hour, per IP.
    pub fn new(http: H) -> Self {
        Self {
            http,
            token: None,
            api_base: GITHUB_API_BASE.to_string(),
            codeload_base: GITHUB_CODELOAD_BASE.to_string(),
            raw_base: GITHUB_RAW_BASE.to_string(),
            rate_limit: Mutex::new(None),
            requests: AtomicUsize::new(0),
        }
    }

    /// A client using [`GITHUB_TOKEN_ENV`] if set, otherwise the token from
    /// `gh auth token`, otherwise none.
    pub fn from_env(http: H) -> Self {
        let (token, _) = crate::github_token::github_credential();
        Self::new(http).with_token(token)
    }

    /// Sets or clears the token.
    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token;
        self
    }

    /// Points the client at other endpoints. For tests.
    pub fn with_endpoints(
        mut self,
        api_base: impl Into<String>,
        codeload_base: impl Into<String>,
        raw_base: impl Into<String>,
    ) -> Self {
        self.api_base = api_base.into().trim_end_matches('/').to_string();
        self.codeload_base = codeload_base.into().trim_end_matches('/').to_string();
        self.raw_base = raw_base.into().trim_end_matches('/').to_string();
        self
    }

    /// True when a token is in use, so 5000 requests an hour rather than 60,
    /// and conditional requests are worth sending.
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// What the last API response said about the rate limit, if anything.
    pub fn rate_limit(&self) -> Option<RateLimit> {
        *self.rate_limit.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The underlying transport.
    pub fn http(&self) -> &H {
        &self.http
    }

    /// How many requests this client has made: API, archive and single file
    /// downloads together.
    ///
    /// The number the interface can quote when it explains why an update check
    /// used part of an hourly budget. Only the API requests spend that budget;
    /// codeload and `raw.githubusercontent.com` are not on it.
    pub fn requests_made(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }

    /// Requests an API URL and turns the reply into a result.
    ///
    /// `etag` is sent as `If-None-Match` only when a token is in use: an
    /// unauthenticated 304 still costs a request out of sixty, so asking
    /// "has it changed?" without a token spends the same quota as asking for
    /// the thing itself.
    fn api(&self, url: &str, etag: Option<&str>) -> Result<HttpResponse, GitHubError> {
        let mut headers: Vec<(&str, &str)> = vec![
            ("Accept", "application/vnd.github+json"),
            ("X-GitHub-Api-Version", API_VERSION),
        ];
        let authorization;
        if let Some(token) = &self.token {
            authorization = format!("Bearer {token}");
            headers.push(("Authorization", &authorization));
            if let Some(etag) = etag {
                headers.push(("If-None-Match", etag));
            }
        }

        self.requests.fetch_add(1, Ordering::Relaxed);
        let response = self.http.get(url, &headers)?;
        if let Some(limit) = RateLimit::from_response(&response) {
            *self.rate_limit.lock().unwrap_or_else(|e| e.into_inner()) = Some(limit);
        }

        match response.status {
            200..=299 | 304 => Ok(response),
            403 | 429 => {
                let limit = RateLimit::from_response(&response);
                if response.header_int("x-ratelimit-remaining") == Some(0)
                    || response.text().contains("rate limit")
                {
                    let limit = limit.unwrap_or(RateLimit {
                        remaining: 0,
                        limit: if self.has_token() { 5000 } else { 60 },
                        reset_unix: 0,
                    });
                    Err(GitHubError::RateLimited {
                        limit: limit.limit,
                        reset: limit.reset_unix,
                    })
                } else {
                    Err(status_error(url, &response))
                }
            }
            404 => Err(GitHubError::NotFound {
                what: url.to_string(),
            }),
            _ => Err(status_error(url, &response)),
        }
    }

    /// Reads an API URL as JSON.
    fn api_json<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T, GitHubError> {
        let response = self.api(url, None)?;
        serde_json::from_slice(&response.body).map_err(|e| GitHubError::Malformed {
            url: url.to_string(),
            detail: e.to_string(),
        })
    }

    /// The commit sha a ref currently points at.
    ///
    /// This is the cheapest question there is — about 371 bytes — and it is the
    /// first step of every update check. A ref that is already a full sha costs
    /// no request at all.
    ///
    /// Branches are asked for first, then tags. An annotated tag points at a
    /// tag object rather than a commit, so that case is dereferenced with one
    /// further request.
    pub fn ref_sha(&self, repo: &RepoRef) -> Result<String, GitHubError> {
        let state = self.ref_state(repo, None)?;
        state.sha.ok_or_else(|| GitHubError::Malformed {
            url: repo.slug(),
            detail: "the ref was reported unchanged without one having been asked about"
                .to_string(),
        })
    }

    /// The commit sha a ref points at, asked conditionally.
    ///
    /// `etag` is the value [`RefState::etag`] returned last time. It is sent as
    /// `If-None-Match` only when a token is in use, because an unauthenticated
    /// 304 still costs one request out of sixty while an authenticated one
    /// costs nothing. GitHub answering 304 comes back as a [`RefState`] with no
    /// sha, meaning "what you already have is current".
    pub fn ref_state(&self, repo: &RepoRef, etag: Option<&str>) -> Result<RefState, GitHubError> {
        if repo.is_sha() {
            return Ok(RefState {
                sha: Some(repo.reference.clone()),
                etag: None,
            });
        }
        let slug = repo.slug();
        let heads = format!(
            "{}/repos/{slug}/git/ref/heads/{}",
            self.api_base, repo.reference
        );
        let response = match self.api(&heads, etag) {
            Ok(response) => response,
            Err(GitHubError::NotFound { .. }) => {
                let tags = format!(
                    "{}/repos/{slug}/git/ref/tags/{}",
                    self.api_base, repo.reference
                );
                match self.api(&tags, etag) {
                    Ok(response) => response,
                    Err(GitHubError::NotFound { .. }) => {
                        return Err(GitHubError::NotFound {
                            what: format!("{slug}@{}", repo.reference),
                        });
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        };

        if response.status == 304 {
            return Ok(RefState {
                sha: None,
                etag: etag.map(str::to_string),
            });
        }
        let seen = response.header("etag").map(str::to_string);
        let object = serde_json::from_slice::<RefResponse>(&response.body)
            .map_err(|e| GitHubError::Malformed {
                url: heads,
                detail: e.to_string(),
            })?
            .object;

        if object.kind == "tag" {
            let url = format!("{}/repos/{slug}/git/tags/{}", self.api_base, object.sha);
            return Ok(RefState {
                sha: Some(self.api_json::<TagResponse>(&url)?.object.sha),
                etag: seen,
            });
        }
        Ok(RefState {
            sha: Some(object.sha),
            etag: seen,
        })
    }

    /// The repository's default branch, one request.
    ///
    /// Only wanted where the branch is genuinely unknown, such as resolving a
    /// skills.sh search result, which records no ref at all.
    pub fn default_branch(&self, owner: &str, repo: &str) -> Result<String, GitHubError> {
        let url = format!("{}/repos/{owner}/{repo}", self.api_base);
        Ok(self.api_json::<RepoResponse>(&url)?.default_branch)
    }

    /// One level of a git tree, or the whole thing when `recursive`.
    ///
    /// `sha` may be a tree sha or a commit sha; GitHub resolves a commit to its
    /// root tree.
    pub fn tree(&self, repo: &RepoRef, sha: &str, recursive: bool) -> Result<Tree, GitHubError> {
        let suffix = if recursive { "?recursive=1" } else { "" };
        let url = format!(
            "{}/repos/{}/git/trees/{sha}{suffix}",
            self.api_base,
            repo.slug()
        );
        self.api_json(&url)
    }

    /// The tree sha of each of `paths`, at `commit_sha`.
    ///
    /// Walks one level at a time and shares every fetched tree between the
    /// paths, so a repository holding six skills under `document-skills/` costs
    /// the root tree plus that one subtree, not six walks.
    ///
    /// A path that names no directory maps to `None` rather than failing, so
    /// one moved skill does not hide the answer for the rest of its repository.
    ///
    /// A truncated response is refetched with `?recursive=1`, which is the only
    /// case where the larger request is worth its size.
    pub fn subtree_shas(
        &self,
        repo: &RepoRef,
        commit_sha: &str,
        paths: &[String],
    ) -> Result<BTreeMap<String, Option<String>>, GitHubError> {
        let mut trees: HashMap<String, Tree> = HashMap::new();
        let mut recursive: Option<Tree> = None;
        let mut out = BTreeMap::new();

        for path in paths {
            let path = path.trim_matches('/').to_string();
            let resolved = self.walk_to(repo, commit_sha, &path, &mut trees, &mut recursive)?;
            out.insert(path, resolved);
        }
        Ok(out)
    }

    /// The tree sha of one path, at `commit_sha`.
    ///
    /// [`GitHubError::PathNotFound`] when the repository holds no directory
    /// there, because a caller asking for one path wants to be told, where
    /// [`GitHub::subtree_shas`] reports the same case as `None`.
    pub fn subtree_sha(
        &self,
        repo: &RepoRef,
        commit_sha: &str,
        path: &str,
    ) -> Result<String, GitHubError> {
        let paths = [path.to_string()];
        let found = self.subtree_shas(repo, commit_sha, &paths)?;
        found
            .into_values()
            .next()
            .flatten()
            .ok_or_else(|| GitHubError::PathNotFound {
                repo: repo.slug(),
                path: path.to_string(),
                reference: repo.reference.clone(),
            })
    }

    /// Walks the tree one segment at a time, reusing trees already fetched.
    fn walk_to(
        &self,
        repo: &RepoRef,
        commit_sha: &str,
        path: &str,
        trees: &mut HashMap<String, Tree>,
        recursive: &mut Option<Tree>,
    ) -> Result<Option<String>, GitHubError> {
        let mut current = commit_sha.to_string();
        if path.is_empty() {
            let tree = self.tree_cached(repo, &current, trees)?;
            return Ok(Some(tree.sha.clone()));
        }

        for segment in path.split('/').filter(|s| !s.is_empty()) {
            let tree = self.tree_cached(repo, &current, trees)?;
            if tree.truncated {
                // The level is incomplete, so an absent entry proves nothing.
                // One recursive fetch answers for every remaining path.
                if recursive.is_none() {
                    *recursive = Some(self.tree(repo, commit_sha, true)?);
                }
                let full = recursive.as_ref().expect("just fetched");
                return Ok(full
                    .tree
                    .iter()
                    .find(|entry| entry.path == path && entry.is_tree())
                    .map(|entry| entry.sha.clone()));
            }
            match tree.entry(segment) {
                Some(entry) if entry.is_tree() => current = entry.sha.clone(),
                _ => return Ok(None),
            }
        }
        Ok(Some(current))
    }

    /// Fetches one tree, or returns the copy already fetched in this walk.
    fn tree_cached<'a>(
        &self,
        repo: &RepoRef,
        sha: &str,
        trees: &'a mut HashMap<String, Tree>,
    ) -> Result<&'a Tree, GitHubError> {
        if !trees.contains_key(sha) {
            let tree = self.tree(repo, sha, false)?;
            trees.insert(sha.to_string(), tree);
        }
        Ok(trees.get(sha).expect("just inserted"))
    }

    /// Every directory in the repository that holds a `SKILL.md`, at
    /// `commit_sha`.
    ///
    /// This is what a repository holds, without knowing any skill's name
    /// beforehand: `anthropics/skills` keeps its skills a directory down, and
    /// nothing in the spelling a user pastes says so. The paths come back
    /// sorted and deduplicated, with the empty string standing for the
    /// repository root when the root itself holds a `SKILL.md`.
    ///
    /// One recursive tree request: the whole repository has to be searched, so
    /// the level-at-a-time walk would cost more, not less.
    pub fn list_skill_dirs(
        &self,
        repo: &RepoRef,
        commit_sha: &str,
    ) -> Result<Vec<String>, GitHubError> {
        let tree = self.tree(repo, commit_sha, true)?;
        let mut found = Vec::new();
        for entry in &tree.tree {
            if !entry.is_blob() || !entry.path.ends_with("SKILL.md") {
                continue;
            }
            match entry.path.rsplit_once('/') {
                Some((dir, "SKILL.md")) => found.push(dir.to_string()),
                Some(_) => continue,
                // A `SKILL.md` at the repository root: the repository is
                // itself the skill.
                None => found.push(String::new()),
            }
        }
        found.sort();
        found.dedup();
        Ok(found)
    }

    /// Every directory named `skill_id` that holds a `SKILL.md`, at
    /// `commit_sha`.
    ///
    /// This is how a skills.sh search result becomes a real location: the
    /// search API returns `owner/repo` and a skill id and no path at all.
    /// The repository root counts as a match when it holds a `SKILL.md` and the
    /// repository is itself the skill. More than one match is returned, in path
    /// order, for the caller to choose between.
    ///
    /// Filters [`GitHub::list_skill_dirs`], so it costs the same one request.
    pub fn find_skill_dirs(
        &self,
        repo: &RepoRef,
        commit_sha: &str,
        skill_id: &str,
    ) -> Result<Vec<String>, GitHubError> {
        let found = self.list_skill_dirs(repo, commit_sha)?;
        Ok(found
            .into_iter()
            .filter(|dir| match dir.rsplit_once('/') {
                Some((_, last)) => last == skill_id,
                // At the root the repository's own name is the id to match;
                // anywhere else the whole path is the last segment.
                None if dir.is_empty() => repo.repo == skill_id,
                None => dir == skill_id,
            })
            .collect())
    }

    /// Downloads the repository archive pinned to `sha`.
    ///
    /// One request, to codeload rather than the API, so it does not spend the
    /// hourly budget. Pinned to a sha rather than a branch, so what is
    /// extracted is exactly what the tree sha recorded as provenance describes.
    pub fn download_tarball(&self, repo: &RepoRef, sha: &str) -> Result<Vec<u8>, GitHubError> {
        let url = format!("{}/{}/tar.gz/{sha}", self.codeload_base, repo.slug());
        let response = self.fetch_bytes(&url)?;
        match response.status {
            200..=299 => Ok(response.body),
            404 => Err(GitHubError::NotFound {
                what: format!("{}@{sha}", repo.slug()),
            }),
            _ => Err(status_error(&url, &response)),
        }
    }

    /// Writes the files of one directory of a repository into `dest`, and
    /// returns how many were written.
    ///
    /// This is the step that gets the bytes, and it prefers the small request.
    /// One recursive tree request lists the wanted directory, and each file
    /// then comes from `raw.githubusercontent.com`, pinned to `commit_sha`.
    /// Installing `skills/pdftk-server` out of `github/awesome-copilot` moves
    /// about 29 KB. Downloading that repository's archive moves 86 MB, which is
    /// past [`MAX_BODY_BYTES`](crate::http::MAX_BODY_BYTES), so before this the
    /// skill could not be installed at all — and every skill in a large
    /// monorepo was in the same position.
    ///
    /// The cost is one API request. Nothing already fetched holds the listing
    /// of the wanted directory, so an unauthenticated install spends four of
    /// its sixty hourly requests rather than three. The file downloads spend
    /// none, and neither does the archive: only `api.github.com` is on the
    /// rate limit.
    ///
    /// Falls back to the whole archive, unpacked by [`extract_subdir`], for the
    /// four cases in [`WholeRepoReason`]. When *that* is refused for its size,
    /// the reason travels with the error, so [`GitHubError::RepoTooLarge`] can
    /// say whether there is a smaller thing to ask for.
    pub fn fetch_files(
        &self,
        repo: &RepoRef,
        commit_sha: &str,
        tree_sha: &str,
        path: &str,
        dest: &Path,
    ) -> Result<usize, GitHubError> {
        let path = path.trim_matches('/');
        let reason = match self.plan_subtree(repo, tree_sha, path)? {
            SubtreePlan::Files(entries) => {
                return self.write_subtree(repo, commit_sha, path, &entries, dest);
            }
            SubtreePlan::WholeArchive(reason) => reason,
        };

        let archive = self.download_tarball(repo, commit_sha).map_err(|e| {
            match e {
                // The one error the user cannot act on as written. It names a
                // codeload URL and a byte count, and neither says that the
                // repository is too big or what to do instead.
                GitHubError::Http(HttpError::TooLarge { limit, .. }) => GitHubError::RepoTooLarge {
                    repo: repo.slug(),
                    path: path.to_string(),
                    limit,
                    reason,
                },
                other => other,
            }
        })?;
        extract_subdir(&archive, path, dest)
    }

    /// Decides whether the wanted directory can be fetched file by file, and
    /// lists it when it can.
    ///
    /// An empty `path` is the repository root: the whole repository is the
    /// skill, so there is nothing smaller to fetch and the archive is the right
    /// request rather than a fallback.
    fn plan_subtree(
        &self,
        repo: &RepoRef,
        tree_sha: &str,
        path: &str,
    ) -> Result<SubtreePlan, GitHubError> {
        if path.is_empty() {
            return Ok(SubtreePlan::WholeArchive(WholeRepoReason::WholeRepository));
        }
        let tree = self.tree(repo, tree_sha, true)?;
        if tree.truncated {
            return Ok(SubtreePlan::WholeArchive(WholeRepoReason::Truncated));
        }
        if tree.tree.iter().any(|entry| entry.is_symlink()) {
            return Ok(SubtreePlan::WholeArchive(WholeRepoReason::HasSymlink));
        }
        if tree.tree.iter().filter(|entry| entry.is_blob()).count() > MAX_SUBTREE_FILES {
            return Ok(SubtreePlan::WholeArchive(WholeRepoReason::TooManyFiles));
        }
        Ok(SubtreePlan::Files(tree.tree))
    }

    /// Downloads each listed file into `dest`, keeping the directory shape.
    ///
    /// The entry paths come from GitHub, but they are checked all the same:
    /// this writes to disk from a remote listing, and [`extract_subdir`] holds
    /// the same line for the same reason. A submodule is skipped, because the
    /// archive holds nothing for one either.
    fn write_subtree(
        &self,
        repo: &RepoRef,
        commit_sha: &str,
        path: &str,
        entries: &[TreeEntry],
        dest: &Path,
    ) -> Result<usize, GitHubError> {
        fs::create_dir_all(dest).map_err(|e| GitHubError::io(dest, e))?;

        let mut written = 0usize;
        for entry in entries {
            let relative = Path::new(&entry.path);
            if entry.path.is_empty() || !is_contained(relative) {
                continue;
            }
            let target = dest.join(relative);
            if entry.is_tree() {
                fs::create_dir_all(&target).map_err(|e| GitHubError::io(&target, e))?;
                continue;
            }
            if !entry.is_blob() {
                continue;
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| GitHubError::io(parent, e))?;
            }

            let url = format!(
                "{}/{}/{commit_sha}/{}",
                self.raw_base,
                repo.slug(),
                encode_path(&format!("{path}/{}", entry.path))
            );
            let response = self.fetch_bytes(&url)?;
            match response.status {
                200..=299 => {}
                404 => {
                    return Err(GitHubError::NotFound {
                        what: format!("{}/{} at {commit_sha}", repo.slug(), entry.path),
                    });
                }
                _ => return Err(status_error(&url, &response)),
            }
            fs::write(&target, &response.body).map_err(|e| GitHubError::io(&target, e))?;
            set_executable(&target, mode_bits(&entry.mode))?;
            written += 1;
        }
        Ok(written)
    }

    /// A `GET` to a host that serves bytes rather than JSON — codeload for an
    /// archive, raw for one file. Neither is on the API rate limit, so neither
    /// carries rate limit headers to record, and a status is left to the
    /// caller, which knows what a 404 means there.
    fn fetch_bytes(&self, url: &str) -> Result<HttpResponse, GitHubError> {
        let mut headers: Vec<(&str, &str)> = Vec::new();
        let authorization;
        if let Some(token) = &self.token {
            authorization = format!("Bearer {token}");
            headers.push(("Authorization", &authorization));
        }
        self.requests.fetch_add(1, Ordering::Relaxed);
        Ok(self.http.get(url, &headers)?)
    }
}

/// What [`GitHub::plan_subtree`] decided.
enum SubtreePlan {
    /// The directory listing, to fetch one file at a time.
    Files(Vec<TreeEntry>),
    /// The directory cannot be fetched that way, for this reason.
    WholeArchive(WholeRepoReason),
}

/// A git file mode as a number. `0` when it is missing or unreadable, which
/// leaves the file its default permissions rather than guessing at them.
fn mode_bits(mode: &str) -> u32 {
    u32::from_str_radix(mode, 8).unwrap_or(0)
}

/// Percent-encodes a repository path for a URL, leaving `/` as the separator.
///
/// A git path may hold anything but a NUL and a slash, and skills in the wild
/// have spaces and `#` in their reference filenames. Unescaped, a `#` truncates
/// the URL at the fragment and the file downloads as the wrong thing.
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Turns a non-2xx response into a [`GitHubError::Status`], pulling out
/// GitHub's own `message` when the body carries one.
fn status_error(url: &str, response: &HttpResponse) -> GitHubError {
    #[derive(Deserialize)]
    struct Message {
        message: String,
    }
    let message = serde_json::from_slice::<Message>(&response.body)
        .map(|m| m.message)
        .unwrap_or_else(|_| {
            let text = response.text();
            text.chars().take(200).collect()
        });
    // An empty body would otherwise render as a sentence ending in a bare
    // colon. Say that GitHub explained nothing, because that is the fact.
    let message = if message.trim().is_empty() {
        "no explanation given".to_string()
    } else {
        message
    };
    GitHubError::Status {
        url: url.to_string(),
        status: response.status,
        message,
    }
}

/// `GET /repos/{o}/{r}/git/ref/...`
#[derive(Deserialize)]
struct RefResponse {
    object: RefObject,
}

/// `GET /repos/{o}/{r}/git/tags/{sha}`
#[derive(Deserialize)]
struct TagResponse {
    object: RefObject,
}

#[derive(Deserialize)]
struct RefObject {
    #[serde(default)]
    sha: String,
    #[serde(rename = "type", default)]
    kind: String,
}

/// `GET /repos/{o}/{r}`, for the one field this crate wants from it.
#[derive(Deserialize)]
struct RepoResponse {
    #[serde(default = "default_branch_name")]
    default_branch: String,
}

fn default_branch_name() -> String {
    DEFAULT_BRANCH.to_string()
}

/// Writes one subdirectory of a GitHub `tar.gz` archive into `dest`.
///
/// GitHub wraps every archive in a single top-level directory named
/// `{repo}-{sha}`. That wrapper is stripped, then only the entries under
/// `subdir` are written, so installing one skill out of a repository holding
/// forty of them writes forty times less. An empty `subdir` extracts the whole
/// repository.
///
/// `dest` is created if missing. Returns how many files were written.
///
/// Refuses anything that would write outside `dest`: an absolute member path,
/// a `..` component, and a symlink whose target climbs out. Those are skipped,
/// not fatal, because one hostile entry should not stop a legitimate skill from
/// installing. Regular files, directories and contained symlinks are extracted;
/// device nodes and hard links are skipped. The executable bit is carried over,
/// because a skill's `scripts/` are meant to run.
pub fn extract_subdir(archive: &[u8], subdir: &str, dest: &Path) -> Result<usize, GitHubError> {
    let subdir = subdir.trim_matches('/');
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(decoder);

    fs::create_dir_all(dest).map_err(|e| GitHubError::io(dest, e))?;

    let mut written = 0usize;
    let entries = tar.entries().map_err(|e| GitHubError::Archive {
        detail: e.to_string(),
    })?;
    for entry in entries {
        let mut entry = entry.map_err(|e| GitHubError::Archive {
            detail: e.to_string(),
        })?;
        let path = entry
            .path()
            .map_err(|e| GitHubError::Archive {
                detail: e.to_string(),
            })?
            .into_owned();

        // Strip GitHub's `{repo}-{sha}/` wrapper.
        let mut components = path.components();
        if components.next().is_none() {
            continue;
        }
        let inner: PathBuf = components.collect();
        let Some(relative) = strip_prefix_path(&inner, subdir) else {
            continue;
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        if !is_contained(&relative) {
            continue;
        }
        let target = dest.join(&relative);

        let kind = entry.header().entry_type();
        if kind.is_dir() {
            fs::create_dir_all(&target).map_err(|e| GitHubError::io(&target, e))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| GitHubError::io(parent, e))?;
        }
        if kind.is_symlink() {
            let link = entry
                .link_name()
                .ok()
                .flatten()
                .map(|p| p.into_owned())
                .unwrap_or_default();
            // A link that leaves the extracted directory is dropped, not
            // followed: the archive is untrusted input.
            if link.is_absolute()
                || !is_contained(&relative.parent().unwrap_or(Path::new("")).join(&link))
            {
                continue;
            }
            let _ = fs::remove_file(&target);
            std::os::unix::fs::symlink(&link, &target).map_err(|e| GitHubError::io(&target, e))?;
            written += 1;
            continue;
        }
        if !kind.is_file() {
            continue;
        }

        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|e| GitHubError::Archive {
                detail: e.to_string(),
            })?;
        fs::write(&target, &bytes).map_err(|e| GitHubError::io(&target, e))?;
        set_executable(&target, entry.header().mode().unwrap_or(0o644))?;
        written += 1;
    }
    Ok(written)
}

/// Gives a file the executable bit when the archive said it had one, and
/// otherwise leaves the default mode alone.
fn set_executable(path: &Path, mode: u32) -> Result<(), GitHubError> {
    use std::os::unix::fs::PermissionsExt as _;
    if mode & 0o111 == 0 {
        return Ok(());
    }
    let metadata = fs::metadata(path).map_err(|e| GitHubError::io(path, e))?;
    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(path, permissions).map_err(|e| GitHubError::io(path, e))
}

/// The part of `path` below `prefix`, or `None` when it is not under it.
/// An empty prefix leaves the path alone.
fn strip_prefix_path(path: &Path, prefix: &str) -> Option<PathBuf> {
    if prefix.is_empty() {
        return Some(path.to_path_buf());
    }
    path.strip_prefix(prefix).ok().map(Path::to_path_buf)
}

/// True when a relative path stays inside the directory it is joined to.
fn is_contained(path: &Path) -> bool {
    let mut depth = 0i32;
    for component in path.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

#[cfg(test)]
pub(crate) mod tests_support {
    //! Archive fixtures shared with the tests of other modules.

    use std::io::Write as _;

    /// A `tar.gz` shaped the way GitHub's is: every member wrapped in a single
    /// top-level `{repo}-{sha}/` directory. Anything under a `scripts/`
    /// directory is given the executable bit, as a real skill's would be.
    ///
    /// Member names are written straight into the header rather than through
    /// `Builder::append_data`, which refuses a `..` component. A hostile
    /// archive is exactly what the extractor has to be tested against, so the
    /// fixture has to be able to build one.
    pub(crate) fn tarball(prefix: &str, files: &[(&str, &str)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(if path.contains("scripts/") {
                0o755
            } else {
                0o644
            });
            let name = format!("{prefix}/{path}");
            let bytes = name.as_bytes();
            let field = &mut header.as_gnu_mut().expect("a GNU header").name;
            assert!(bytes.len() < field.len(), "fixture name is too long");
            field[..bytes.len()].copy_from_slice(bytes);
            header.set_cksum();
            builder
                .append(&header, contents.as_bytes())
                .expect("append");
        }
        let tar = builder.into_inner().expect("finish tar");
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&tar).expect("compress");
        encoder.finish().expect("finish gzip")
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::tarball;
    use super::*;
    use crate::http::fake::FakeHttp;

    fn client(http: FakeHttp) -> GitHub<FakeHttp> {
        GitHub::new(http).with_endpoints(
            "https://api.test",
            "https://codeload.test",
            "https://raw.test",
        )
    }

    #[test]
    fn a_location_is_read_from_every_spelling() {
        let cases = [
            ("anthropics/skills", "main", ""),
            ("anthropics/skills@v2", "v2", ""),
            ("https://github.com/anthropics/skills", "main", ""),
            (
                "https://github.com/anthropics/skills/tree/next/document-skills/pdf",
                "next",
                "document-skills/pdf",
            ),
            (
                "anthropics/skills/document-skills/pdf",
                "main",
                "document-skills/pdf",
            ),
            ("git@github.com:anthropics/skills.git", "main", ""),
        ];
        for (spec, reference, path) in cases {
            let location = SkillLocation::parse(spec).unwrap_or_else(|| panic!("{spec}"));
            assert_eq!(location.repo.owner, "anthropics", "{spec}");
            assert_eq!(location.repo.repo, "skills", "{spec}");
            assert_eq!(location.repo.reference, reference, "{spec}");
            assert_eq!(location.path, path, "{spec}");
        }
        assert_eq!(SkillLocation::parse("anthropics"), None);
    }

    #[test]
    fn parsing_says_whether_the_spec_named_the_ref() {
        let cases = [
            ("o/r", false, "main", ""),
            ("o/r@master", true, "master", ""),
            ("o/r/a/b@v1", true, "v1", "a/b"),
            (
                "https://github.com/o/r/tree/dev/skills/pdf",
                true,
                "dev",
                "skills/pdf",
            ),
            // The `@` in an SSH URL is part of the host, not a ref.
            ("git@github.com:o/r.git", false, "main", ""),
        ];
        for (spec, explicit, reference, path) in cases {
            let parsed = SkillLocation::parse_spec(spec).unwrap_or_else(|| panic!("{spec}"));
            assert_eq!(parsed.explicit_ref, explicit, "{spec}");
            assert_eq!(parsed.location.repo.reference, reference, "{spec}");
            assert_eq!(parsed.location.path, path, "{spec}");
            assert_eq!(
                SkillLocation::parse(spec).as_ref(),
                Some(&parsed.location),
                "{spec}"
            );
        }
        assert_eq!(SkillLocation::parse_spec("anthropics"), None);
    }

    #[test]
    fn dir_name_falls_back_to_the_repository_at_the_root() {
        assert_eq!(
            SkillLocation::parse("o/my-skill").unwrap().dir_name(),
            "my-skill"
        );
        assert_eq!(SkillLocation::parse("o/r/a/pdf").unwrap().dir_name(), "pdf");
    }

    #[test]
    fn a_ref_that_is_already_a_sha_costs_no_request() {
        let http = FakeHttp::new();
        let gh = client(http);
        let repo = RepoRef::new("o", "r", "a".repeat(40));
        assert_eq!(gh.ref_sha(&repo).unwrap(), "a".repeat(40));
        assert_eq!(gh.http().request_count(), 0);
    }

    #[test]
    fn a_branch_ref_is_one_request() {
        let http = FakeHttp::new();
        http.json(
            "https://api.test/repos/o/r/git/ref/heads/main",
            r#"{"ref":"refs/heads/main","object":{"sha":"c0ffee","type":"commit"}}"#,
        );
        let gh = client(http);
        let sha = gh.ref_sha(&RepoRef::new("o", "r", "main")).unwrap();
        assert_eq!(sha, "c0ffee");
        assert_eq!(gh.http().request_count(), 1);
    }

    #[test]
    fn a_tag_is_tried_when_no_branch_matches_and_annotated_tags_are_dereferenced() {
        let http = FakeHttp::new();
        http.json(
            "https://api.test/repos/o/r/git/ref/tags/v1",
            r#"{"object":{"sha":"tagobj","type":"tag"}}"#,
        );
        http.json(
            "https://api.test/repos/o/r/git/tags/tagobj",
            r#"{"object":{"sha":"commitsha","type":"commit"}}"#,
        );
        let gh = client(http);
        assert_eq!(
            gh.ref_sha(&RepoRef::new("o", "r", "v1")).unwrap(),
            "commitsha"
        );
        assert_eq!(
            gh.http().urls(),
            [
                "https://api.test/repos/o/r/git/ref/heads/v1",
                "https://api.test/repos/o/r/git/ref/tags/v1",
                "https://api.test/repos/o/r/git/tags/tagobj",
            ]
        );
    }

    #[test]
    fn a_rate_limited_response_surfaces_the_reset_time() {
        let http = FakeHttp::new();
        http.reply(
            "https://api.test/repos/o/r/git/ref/heads/main",
            HttpResponse::new(403, br#"{"message":"API rate limit exceeded"}"#.to_vec())
                .with_header("x-ratelimit-limit", "60")
                .with_header("x-ratelimit-remaining", "0")
                .with_header("x-ratelimit-reset", "1800000000"),
        );
        let gh = client(http);
        let err = gh.ref_sha(&RepoRef::new("o", "r", "main")).unwrap_err();
        assert!(err.is_rate_limited());
        match err {
            GitHubError::RateLimited { limit, reset } => {
                assert_eq!(limit, 60);
                assert_eq!(reset, 1_800_000_000);
            }
            other => panic!("{other:?}"),
        }
        let limit = gh.rate_limit().expect("rate limit read from headers");
        assert!(limit.is_exhausted());
        assert_eq!(
            limit.reset_at(),
            Some(UNIX_EPOCH + Duration::from_secs(1_800_000_000))
        );
        assert_eq!(
            limit.wait_from(UNIX_EPOCH + Duration::from_secs(1_799_999_940)),
            Duration::from_secs(60)
        );
        assert_eq!(
            limit.wait_from(UNIX_EPOCH + Duration::from_secs(1_900_000_000)),
            Duration::ZERO
        );
    }

    #[test]
    fn an_etag_is_only_sent_with_a_token() {
        let http = FakeHttp::new();
        http.json("https://api.test/repos/o/r", r#"{"default_branch":"main"}"#);
        let gh = client(http);
        gh.default_branch("o", "r").unwrap();
        let sent = gh.http().requests();
        assert!(sent[0].headers.iter().all(|(k, _)| k != "Authorization"));
    }

    #[test]
    fn walking_two_levels_shares_the_trees_between_paths() {
        let http = FakeHttp::new();
        http.json(
            "https://api.test/repos/o/r/git/trees/commit1",
            r#"{"sha":"root","truncated":false,"tree":[
                {"path":"document-skills","mode":"040000","type":"tree","sha":"docs"},
                {"path":"README.md","mode":"100644","type":"blob","sha":"b1"}]}"#,
        );
        http.json(
            "https://api.test/repos/o/r/git/trees/docs",
            r#"{"sha":"docs","truncated":false,"tree":[
                {"path":"pdf","mode":"040000","type":"tree","sha":"pdfsha"},
                {"path":"docx","mode":"040000","type":"tree","sha":"docxsha"}]}"#,
        );
        let gh = client(http);
        let repo = RepoRef::new("o", "r", "main");
        let found = gh
            .subtree_shas(
                &repo,
                "commit1",
                &[
                    "document-skills/pdf".to_string(),
                    "document-skills/docx".to_string(),
                    "document-skills/missing".to_string(),
                    String::new(),
                ],
            )
            .unwrap();
        assert_eq!(found["document-skills/pdf"].as_deref(), Some("pdfsha"));
        assert_eq!(found["document-skills/docx"].as_deref(), Some("docxsha"));
        assert_eq!(found["document-skills/missing"], None);
        assert_eq!(found[""].as_deref(), Some("root"));
        // Two trees fetched, however many paths were asked for.
        assert_eq!(gh.http().request_count(), 2);
    }

    #[test]
    fn a_truncated_tree_falls_back_to_the_recursive_form() {
        let http = FakeHttp::new();
        http.json(
            "https://api.test/repos/o/r/git/trees/commit1",
            r#"{"sha":"root","truncated":true,"tree":[]}"#,
        );
        http.json(
            "https://api.test/repos/o/r/git/trees/commit1?recursive=1",
            r#"{"sha":"root","truncated":false,"tree":[
                {"path":"a","type":"tree","sha":"asha"},
                {"path":"a/pdf","type":"tree","sha":"pdfsha"}]}"#,
        );
        let gh = client(http);
        let repo = RepoRef::new("o", "r", "main");
        let sha = gh.subtree_sha(&repo, "commit1", "a/pdf").unwrap();
        assert_eq!(sha, "pdfsha");
        assert!(
            gh.http()
                .urls()
                .contains(&"https://api.test/repos/o/r/git/trees/commit1?recursive=1".to_string())
        );
    }

    #[test]
    fn a_missing_path_is_named_in_the_error() {
        let http = FakeHttp::new();
        http.json(
            "https://api.test/repos/o/r/git/trees/commit1",
            r#"{"sha":"root","truncated":false,"tree":[]}"#,
        );
        let gh = client(http);
        let err = gh
            .subtree_sha(&RepoRef::new("o", "r", "main"), "commit1", "nope")
            .unwrap_err();
        assert!(matches!(err, GitHubError::PathNotFound { .. }), "{err:?}");
    }

    #[test]
    fn skill_directories_are_found_by_name_including_the_repository_root() {
        let http = FakeHttp::new();
        http.json(
            "https://api.test/repos/o/pdf/git/trees/c1?recursive=1",
            r#"{"sha":"root","truncated":false,"tree":[
                {"path":"SKILL.md","type":"blob","sha":"b0"},
                {"path":"a","type":"tree","sha":"t1"},
                {"path":"a/pdf","type":"tree","sha":"t2"},
                {"path":"a/pdf/SKILL.md","type":"blob","sha":"b1"},
                {"path":"b/pdf/SKILL.md","type":"blob","sha":"b2"},
                {"path":"b/other/SKILL.md","type":"blob","sha":"b3"}]}"#,
        );
        let gh = client(http);
        let found = gh
            .find_skill_dirs(&RepoRef::new("o", "pdf", "main"), "c1", "pdf")
            .unwrap();
        assert_eq!(found, ["", "a/pdf", "b/pdf"]);
    }

    #[test]
    fn every_skill_in_a_repository_is_listed_whatever_it_is_called() {
        let http = FakeHttp::new();
        http.json(
            "https://api.test/repos/o/skills/git/trees/c1?recursive=1",
            r#"{"sha":"root","truncated":false,"tree":[
                {"path":"README.md","type":"blob","sha":"b0"},
                {"path":"document-skills","type":"tree","sha":"t1"},
                {"path":"document-skills/pdf","type":"tree","sha":"t2"},
                {"path":"document-skills/pdf/SKILL.md","type":"blob","sha":"b1"},
                {"path":"document-skills/pdf/scripts/run.sh","type":"blob","sha":"b2"},
                {"path":"document-skills/docx/SKILL.md","type":"blob","sha":"b3"},
                {"path":"artifacts/deep/nested/SKILL.md","type":"blob","sha":"b4"},
                {"path":"artifacts/notes.md","type":"blob","sha":"b5"}]}"#,
        );
        let gh = client(http);
        let found = gh
            .list_skill_dirs(&RepoRef::new("o", "skills", "main"), "c1")
            .unwrap();
        assert_eq!(
            found,
            [
                "artifacts/deep/nested",
                "document-skills/docx",
                "document-skills/pdf",
            ]
        );
        // One recursive tree request answers for the whole repository.
        assert_eq!(gh.http().request_count(), 1);
    }

    #[test]
    fn a_skill_at_the_repository_root_is_listed_as_the_empty_path() {
        let http = FakeHttp::new();
        http.json(
            "https://api.test/repos/o/pdf/git/trees/c1?recursive=1",
            r#"{"sha":"root","truncated":false,"tree":[
                {"path":"SKILL.md","type":"blob","sha":"b0"},
                {"path":"examples/one/SKILL.md","type":"blob","sha":"b1"}]}"#,
        );
        let gh = client(http);
        let found = gh
            .list_skill_dirs(&RepoRef::new("o", "pdf", "main"), "c1")
            .unwrap();
        assert_eq!(found, ["", "examples/one"]);
    }

    #[test]
    fn extraction_pulls_out_only_the_requested_subdirectory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let archive = tarball(
            "skills-abc123",
            &[
                ("README.md", "not the skill\n"),
                ("document-skills/pdf/SKILL.md", "---\nname: pdf\n---\n"),
                ("document-skills/pdf/scripts/run.sh", "#!/bin/sh\necho hi\n"),
                ("document-skills/docx/SKILL.md", "---\nname: docx\n---\n"),
                ("other/thing.txt", "no\n"),
            ],
        );
        let dest = dir.path().join("out");
        let written = extract_subdir(&archive, "document-skills/pdf", &dest).unwrap();
        assert_eq!(written, 2);

        assert!(dest.join("SKILL.md").is_file());
        assert!(dest.join("scripts/run.sh").is_file());
        assert!(!dest.join("README.md").exists());
        assert!(!dest.join("docx").exists());
        assert!(!dest.join("document-skills").exists());

        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(dest.join("scripts/run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "the executable bit survives");
    }

    #[test]
    fn extraction_refuses_a_member_that_would_escape_the_destination() {
        let dir = tempfile::tempdir().expect("temp dir");
        let archive = tarball(
            "skills-abc123",
            &[("pdf/SKILL.md", "ok\n"), ("pdf/../../escaped.txt", "no\n")],
        );
        let dest = dir.path().join("out");
        let written = extract_subdir(&archive, "pdf", &dest).unwrap();
        assert_eq!(written, 1);
        assert!(dest.join("SKILL.md").is_file());
        assert!(!dir.path().join("escaped.txt").exists());
    }

    #[test]
    fn an_empty_subdirectory_extracts_the_whole_repository() {
        let dir = tempfile::tempdir().expect("temp dir");
        let archive = tarball("r-sha", &[("SKILL.md", "x\n"), ("a/b.txt", "y\n")]);
        let dest = dir.path().join("out");
        assert_eq!(extract_subdir(&archive, "", &dest).unwrap(), 2);
        assert!(dest.join("SKILL.md").is_file());
        assert!(dest.join("a/b.txt").is_file());
    }

    #[test]
    fn a_tarball_is_fetched_from_codeload_pinned_to_a_sha() {
        let http = FakeHttp::new();
        http.reply(
            "https://codeload.test/o/r/tar.gz/c0ffee",
            HttpResponse::new(200, b"gzip bytes".to_vec()),
        );
        let gh = client(http);
        let bytes = gh
            .download_tarball(&RepoRef::new("o", "r", "main"), "c0ffee")
            .unwrap();
        assert_eq!(bytes, b"gzip bytes");
    }

    // -- fetching the files --------------------------------------------------

    fn repo() -> RepoRef {
        RepoRef::new("o", "r", "main")
    }

    /// A listing of `skills/pdf`, with whatever entries the test needs, plus
    /// the archive that the fallback would download instead.
    fn fetch_http(listing: &str) -> FakeHttp {
        let http = FakeHttp::new();
        http.json(
            "https://api.test/repos/o/r/git/trees/pdf-tree?recursive=1",
            listing,
        );
        http.reply(
            "https://raw.test/o/r/c0ffee/skills/pdf/SKILL.md",
            HttpResponse::new(200, "---\nname: pdf\n---\n"),
        );
        http.reply(
            "https://raw.test/o/r/c0ffee/skills/pdf/scripts/run.sh",
            HttpResponse::new(200, "#!/bin/sh\necho hi\n"),
        );
        http.reply(
            "https://codeload.test/o/r/tar.gz/c0ffee",
            HttpResponse::new(
                200,
                tarball(
                    "r-c0ffee",
                    &[
                        ("README.md", "not the skill\n"),
                        ("skills/pdf/SKILL.md", "---\nname: pdf\n---\n"),
                        ("skills/pdf/scripts/run.sh", "#!/bin/sh\necho hi\n"),
                    ],
                ),
            ),
        );
        http
    }

    const PDF_LISTING: &str = r#"{"sha":"pdf-tree","truncated":false,"tree":[
        {"path":"SKILL.md","mode":"100644","type":"blob","sha":"b1"},
        {"path":"scripts","mode":"040000","type":"tree","sha":"t1"},
        {"path":"scripts/run.sh","mode":"100755","type":"blob","sha":"b2"}]}"#;

    /// The fix for a skill that could not be installed at all: fetching only
    /// the skill's directory never asks for the repository archive, so the
    /// archive's size stops mattering.
    #[test]
    fn a_subtree_is_fetched_file_by_file_and_never_asks_for_the_archive() {
        let dir = tempfile::tempdir().expect("temp dir");
        let dest = dir.path().join("out");
        let gh = client(fetch_http(PDF_LISTING));

        let written = gh
            .fetch_files(&repo(), "c0ffee", "pdf-tree", "skills/pdf", &dest)
            .unwrap();

        assert_eq!(written, 2, "two blobs, and the directory is not a file");
        assert_eq!(
            fs::read_to_string(dest.join("SKILL.md")).unwrap(),
            "---\nname: pdf\n---\n"
        );
        assert!(dest.join("scripts/run.sh").is_file());
        assert!(
            gh.http().urls().iter().all(|url| !url.contains("codeload")),
            "the archive was downloaded anyway: {:?}",
            gh.http().urls()
        );

        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(dest.join("scripts/run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "mode 100755 keeps the executable bit");
    }

    /// The repository root is the one case where the whole archive is the
    /// right request rather than a fallback: there is nothing smaller to ask
    /// for.
    #[test]
    fn the_repository_root_is_fetched_as_the_whole_archive() {
        let dir = tempfile::tempdir().expect("temp dir");
        let dest = dir.path().join("out");
        let gh = client(fetch_http(PDF_LISTING));

        let written = gh
            .fetch_files(&repo(), "c0ffee", "root-tree", "", &dest)
            .unwrap();

        assert_eq!(written, 3);
        assert!(dest.join("README.md").is_file());
        assert!(
            gh.http().urls().iter().any(|url| url.contains("codeload")),
            "the root has to come from the archive"
        );
    }

    /// A symlink is the one shape the file-by-file fetch will not recreate:
    /// only [`extract_subdir`] checks where the link points before writing it.
    #[test]
    fn a_subtree_holding_a_symlink_falls_back_to_the_archive() {
        let dir = tempfile::tempdir().expect("temp dir");
        let dest = dir.path().join("out");
        let gh = client(fetch_http(
            r#"{"sha":"pdf-tree","truncated":false,"tree":[
                {"path":"SKILL.md","mode":"100644","type":"blob","sha":"b1"},
                {"path":"link","mode":"120000","type":"blob","sha":"b3"}]}"#,
        ));

        gh.fetch_files(&repo(), "c0ffee", "pdf-tree", "skills/pdf", &dest)
            .unwrap();

        assert!(
            gh.http().urls().iter().any(|url| url.contains("codeload")),
            "a symlink sends it to the archive"
        );
    }

    #[test]
    fn a_directory_of_too_many_files_falls_back_to_the_archive() {
        let dir = tempfile::tempdir().expect("temp dir");
        let dest = dir.path().join("out");
        let entries: Vec<String> = (0..=MAX_SUBTREE_FILES)
            .map(|n| format!(r#"{{"path":"f{n}","mode":"100644","type":"blob","sha":"b{n}"}}"#))
            .collect();
        let gh = client(fetch_http(&format!(
            r#"{{"sha":"pdf-tree","truncated":false,"tree":[{}]}}"#,
            entries.join(",")
        )));

        gh.fetch_files(&repo(), "c0ffee", "pdf-tree", "skills/pdf", &dest)
            .unwrap();

        assert!(gh.http().urls().iter().any(|url| url.contains("codeload")));
    }

    /// A truncated listing is not evidence of anything: a file missing from it
    /// may still be in the repository, so the archive is the only complete
    /// answer.
    #[test]
    fn a_truncated_listing_falls_back_to_the_archive() {
        let dir = tempfile::tempdir().expect("temp dir");
        let dest = dir.path().join("out");
        let gh = client(fetch_http(
            r#"{"sha":"pdf-tree","truncated":true,"tree":[
                {"path":"SKILL.md","mode":"100644","type":"blob","sha":"b1"}]}"#,
        ));

        gh.fetch_files(&repo(), "c0ffee", "pdf-tree", "skills/pdf", &dest)
            .unwrap();

        assert!(gh.http().urls().iter().any(|url| url.contains("codeload")));
    }

    /// The message issue 12 is about. Asking for the whole repository has a
    /// real next step, so the sentence names it and spells it the way the
    /// field takes it.
    #[test]
    fn a_repository_too_large_to_download_whole_says_to_name_one_skill() {
        let dir = tempfile::tempdir().expect("temp dir");
        let dest = dir.path().join("out");
        let http = fetch_http(PDF_LISTING);
        http.too_large("https://codeload.test/o/r/tar.gz/c0ffee");
        let gh = client(http);

        let error = gh
            .fetch_files(&repo(), "c0ffee", "root-tree", "", &dest)
            .unwrap_err();

        assert!(
            matches!(
                error,
                GitHubError::RepoTooLarge {
                    reason: WholeRepoReason::WholeRepository,
                    ..
                }
            ),
            "{error:?}"
        );
        let message = error.to_string();
        assert_eq!(
            message,
            "o/r is too large to download whole (over 64 MB). Install one skill from it \
             instead of the whole repository, by naming that skill's directory: \
             o/r/path/to/skill."
        );
        assert!(
            !message.contains("bytes") && !message.contains("codeload"),
            "no byte counts and no URLs: {message}"
        );
    }

    /// The other half: a directory that could not be fetched on its own has no
    /// shorter download to offer, so the message says why and points at the
    /// one thing that does work.
    #[test]
    fn a_directory_that_needed_the_archive_says_why_it_could_not_be_fetched_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let dest = dir.path().join("out");
        let http = fetch_http(
            r#"{"sha":"pdf-tree","truncated":false,"tree":[
                {"path":"link","mode":"120000","type":"blob","sha":"b3"}]}"#,
        );
        http.too_large("https://codeload.test/o/r/tar.gz/c0ffee");
        let gh = client(http);

        let error = gh
            .fetch_files(&repo(), "c0ffee", "pdf-tree", "skills/pdf", &dest)
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "o/r is too large to download whole (over 64 MB), and `skills/pdf` could not be \
             fetched on its own because it holds a symlink. Clone the repository and install \
             the skill from that folder instead."
        );
    }

    /// A `#` unescaped would truncate the URL at the fragment and download the
    /// wrong file; a space would not be a URL at all.
    #[test]
    fn a_file_name_that_is_not_url_safe_is_encoded() {
        assert_eq!(
            encode_path("skills/pdf/refs/a b#c.md"),
            "skills/pdf/refs/a%20b%23c.md"
        );
        assert_eq!(encode_path("a/b-c_d.e~f"), "a/b-c_d.e~f");
    }

    /// The listing comes from GitHub, but it is still remote input that this
    /// writes to disk from.
    #[test]
    fn a_listed_path_that_would_escape_the_destination_is_skipped() {
        let dir = tempfile::tempdir().expect("temp dir");
        let dest = dir.path().join("out");
        let gh = client(fetch_http(
            r#"{"sha":"pdf-tree","truncated":false,"tree":[
                {"path":"SKILL.md","mode":"100644","type":"blob","sha":"b1"},
                {"path":"../escaped.txt","mode":"100644","type":"blob","sha":"b9"}]}"#,
        ));

        let written = gh
            .fetch_files(&repo(), "c0ffee", "pdf-tree", "skills/pdf", &dest)
            .unwrap();

        assert_eq!(written, 1);
        assert!(dest.join("SKILL.md").is_file());
        assert!(!dir.path().join("escaped.txt").exists());
    }
}
