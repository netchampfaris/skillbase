//! Read, model and write Agent Skills.
//!
//! A skill is a *directory* holding a `SKILL.md` at its root, plus whatever
//! files it bundles (`scripts/`, `references/`, `assets/`). `SKILL.md` is
//! Markdown with a YAML frontmatter block:
//!
//! ```text
//! ---
//! name: my-skill
//! description: When and what this skill does.
//! license: Apache-2.0
//! allowed-tools: Read, Grep, Bash
//! metadata:
//!   version: "1.0"
//! ---
//!
//! # My Skill
//! ```
//!
//! `name` and `description` are required. Everything else is optional and
//! varies by agent, so this crate treats unknown keys as data to be preserved,
//! not noise to be dropped.
//!
//! # Fidelity
//!
//! [`SkillDoc::parse`] followed by [`SkillDoc::to_markdown`] reproduces the
//! source byte for byte — CRLF, byte-order mark, YAML comments, quoting style,
//! block scalars, a missing trailing newline and all. A parsed document keeps
//! its delimiter lines, its frontmatter source and its body as verbatim
//! strings, and only re-serializes the YAML once the frontmatter is mutated.
//!
//! After a mutation the mapping is re-emitted. Key order still survives,
//! because `serde_yaml_ng::Mapping` is an `IndexMap` and this crate replaces
//! values in place and uses shift-removal. What does not survive re-emission is
//! YAML *spelling*: comments, blank lines, quote style, block scalars and
//! indentation are normalized. [`SkillDoc::to_markdown`] lists each case. The
//! Markdown body is never re-rendered.
//!
//! # Parsing and validation are separate
//!
//! [`SkillDoc::parse`] fails only when the text is not structurally a
//! frontmatter document. A missing `name`, a name in the wrong case, an
//! over-long description: none of those stop it. They come back from
//! [`SkillDoc::validate`] as [`ValidationIssue`]s instead, so an editor can
//! open a broken skill, mark it invalid and let the user repair it. A parse
//! error means there is nothing to show; a validation error means there is
//! something to fix.
//!
//! # Example
//!
//! ```
//! use skillbase_core::SkillDoc;
//!
//! let source = "---\nname: my-skill\nx-agent-key: kept\ndescription: Old.\n---\n# Body\n";
//! let mut doc = SkillDoc::parse(source)?;
//! assert_eq!(doc.frontmatter.name(), Some("my-skill"));
//! assert_eq!(doc.to_markdown(), source);
//!
//! doc.frontmatter.set_description("New.");
//! let updated = doc.to_markdown();
//! assert!(updated.contains("x-agent-key: kept"));
//! assert_eq!(
//!     SkillDoc::parse(&updated)?.frontmatter.keys().collect::<Vec<_>>(),
//!     ["name", "x-agent-key", "description"],
//! );
//! # Ok::<(), skillbase_core::SkillError>(())
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod discovery;
mod doc;
mod error;
mod frontmatter;
mod github;
mod github_token;
mod http;
mod install;
mod provenance;
mod registry;
mod remote;
mod skill;
mod skillsh;
mod slug;
#[cfg(test)]
mod test_fixture;
mod usage;

#[cfg(not(unix))]
compile_error!(
    "skillbase-core targets macOS and Linux: making a skill visible to an agent \
     needs unix symlinks, which this platform does not provide."
);

pub use discovery::{
    DiscoveredSkill, Discovery, DiscoveryResult, Location, LocationKind, agent_of,
    codex_disabled_paths, scan,
};
pub use doc::{LineEnding, SkillDoc};
pub use error::{MAX_DESCRIPTION_LEN, MAX_NAME_LEN, Severity, SkillError, ValidationIssue};
pub use frontmatter::{
    KEY_ALLOWED_TOOLS, KEY_DESCRIPTION, KEY_LICENSE, KEY_METADATA, KEY_NAME, KEY_VERSION,
    SkillFrontmatter,
};
pub use github::{
    GITHUB_API_BASE, GITHUB_CODELOAD_BASE, GITHUB_TOKEN_ENV, GitHub, GitHubError, ParsedLocation,
    RateLimit, RefState, RepoRef, SkillLocation, Tree, TreeEntry, extract_subdir,
};
pub use github_token::{TokenSource, github_token_source, refresh_github_token};
pub use http::{Http, HttpError, HttpResponse, MAX_BODY_BYTES, USER_AGENT, UreqHttp};
pub use install::{
    COPY_MARKER, Change, ConsolidatePlan, ContentDiff, DeletePlan, Duplicate, ImportOptions,
    InstallError, Installer, Outcome,
};
pub use provenance::{
    DEFAULT_BRANCH, KEY_GITHUB_PATH, KEY_GITHUB_REF, KEY_GITHUB_REPO, KEY_GITHUB_TREE_SHA,
    LockEntry, PROVENANCE_KEYS, Provenance, SKILL_LOCK_FILE, SkillLock, split_owner_repo,
};
pub use registry::{
    AgentDef, DisableMode, GlobalDir, LinkMode, PRIVATE_DIR, PRIVATE_ID, Presence, Registry, Roots,
    SHARED_ID, SHARED_SKILLS_DIR, STORE_DIR, UNSUPPORTED, home_dir,
};
pub use remote::{
    CacheWriteError, FetchError, InstallOptions, Installed, LocalState, REMOTE_CACHE_FILE,
    RemoteCache, RepoRecord, STAGING_DIR, SkillRecord, StagingSweep, UpdateReport, UpdateStatus,
    UpdateTarget, check_updates, content_digest, install_from_github, local_state, provenance_of,
    repo_key, sweep_staging, update_targets, upstream_state,
};
pub use skill::{SKILL_FILE_NAME, Skill};
pub use skillsh::{
    DEFAULT_LIMIT, MAX_LIMIT, MIN_QUERY_LEN, SEARCH_URL, SearchError, SearchHit, SearchResults,
    SkillsSh, resolve,
};
pub use slug::{FALLBACK_SLUG, is_kebab_case, slugify};
pub use usage::{
    CLAUDE_RETENTION_DAYS, CLAUDE_TRANSCRIPT_DIR, COPILOT_SESSION_DIR, RECORDING_AGENT_IDS,
    SourceStat, USAGE_CACHE_FILE, Usage, UsageSource,
};

/// Re-exported so callers can build and inspect frontmatter values without
/// having to depend on the same `serde_yaml_ng` version themselves.
pub use serde_yaml_ng::{Mapping, Number, Value};
