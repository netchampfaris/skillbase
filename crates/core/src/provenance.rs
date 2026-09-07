//! Where an installed skill came from.
//!
//! Two sources of that fact, read the same way:
//!
//! * Keys Skillbase writes into the installed `SKILL.md` under `metadata`.
//!   They match the ones `gh skill install` writes, so a skill installed by
//!   either tool is recognised by the other.
//! * `~/.agents/.skill-lock.json`, the lockfile `npx skills` maintains. It is
//!   read and never written, so skills that tool installed are still seen as
//!   updatable.
//!
//! # Why `metadata` and not the top level
//!
//! The Agent Skills format allows exactly one place for fields it does not
//! define: the `metadata` mapping. A top-level `version:` or `source:` is a
//! hard validation error in the agents that check, so nothing here is ever
//! written at the top level. Unknown keys inside `metadata` round-trip through
//! [`SkillFrontmatter`] untouched, so writing four of them disturbs nothing
//! else in the file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_yaml_ng::{Mapping, Value};

use crate::frontmatter::{KEY_METADATA, SkillFrontmatter};
use crate::registry::Roots;

/// Metadata key holding the repository URL, e.g.
/// `https://github.com/anthropics/skills`.
pub const KEY_GITHUB_REPO: &str = "github-repo";

/// Metadata key holding the branch or tag the skill was installed from.
pub const KEY_GITHUB_REF: &str = "github-ref";

/// Metadata key holding the git tree sha of the skill's subdirectory.
pub const KEY_GITHUB_TREE_SHA: &str = "github-tree-sha";

/// Metadata key holding the skill's subdirectory path within the repository.
pub const KEY_GITHUB_PATH: &str = "github-path";

/// The lockfile `npx skills` maintains, relative to the home directory.
pub const SKILL_LOCK_FILE: &str = ".agents/.skill-lock.json";

/// Every metadata key this module owns, in the order it writes them.
pub const PROVENANCE_KEYS: [&str; 4] = [
    KEY_GITHUB_REPO,
    KEY_GITHUB_REF,
    KEY_GITHUB_TREE_SHA,
    KEY_GITHUB_PATH,
];

/// The GitHub location an installed skill came from.
///
/// `tree_sha` is what answers "has upstream changed?": it is compared against
/// the tree sha GitHub reports for the same path now, with no local hashing.
/// It is optional because the `npx skills` lockfile records no sha, so a skill
/// that tool installed has a known location and no baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// The repository URL, e.g. `https://github.com/anthropics/skills`.
    pub repo_url: String,
    /// The branch or tag as requested at install time.
    pub reference: String,
    /// The git tree sha of the skill's subdirectory at install time.
    pub tree_sha: Option<String>,
    /// The skill's subdirectory within the repository. Empty at the repo root.
    pub path: String,
}

impl Provenance {
    /// A provenance with every field known.
    pub fn new(
        repo_url: impl Into<String>,
        reference: impl Into<String>,
        tree_sha: impl Into<String>,
        path: impl Into<String>,
    ) -> Self {
        Self {
            repo_url: repo_url.into(),
            reference: reference.into(),
            tree_sha: Some(tree_sha.into()),
            path: path.into(),
        }
    }

    /// The `owner` and `repo` parts of [`Provenance::repo_url`].
    ///
    /// Accepts a full URL, an `owner/repo` slug, and a `git@github.com:` remote,
    /// with or without a `.git` suffix.
    pub fn owner_repo(&self) -> Option<(String, String)> {
        split_owner_repo(&self.repo_url)
    }

    /// Reads the four keys from `metadata`.
    ///
    /// Returns `None` when the repository URL is absent, because a path or a
    /// sha with no repository names nothing. A missing `github-ref` defaults to
    /// [`DEFAULT_BRANCH`]; a missing `github-path` means the repository root.
    pub fn read(frontmatter: &SkillFrontmatter) -> Option<Provenance> {
        let metadata = frontmatter.metadata()?;
        let repo_url = metadata_str(metadata, KEY_GITHUB_REPO)?;
        Some(Provenance {
            repo_url,
            reference: metadata_str(metadata, KEY_GITHUB_REF)
                .unwrap_or_else(|| DEFAULT_BRANCH.to_string()),
            tree_sha: metadata_str(metadata, KEY_GITHUB_TREE_SHA),
            path: metadata_str(metadata, KEY_GITHUB_PATH).unwrap_or_default(),
        })
    }

    /// Writes the four keys into `metadata`, creating that mapping when the
    /// frontmatter has none.
    ///
    /// Every other key keeps its value and its position, inside `metadata` and
    /// outside it. A key already present is replaced in place rather than
    /// appended, so re-installing a skill does not reorder its frontmatter.
    /// `github-tree-sha` is removed rather than written when this provenance
    /// has none, so a stale sha is never left behind.
    pub fn write(&self, frontmatter: &mut SkillFrontmatter) {
        let mut metadata = frontmatter.metadata().cloned().unwrap_or_else(Mapping::new);
        set(&mut metadata, KEY_GITHUB_REPO, Some(&self.repo_url));
        set(&mut metadata, KEY_GITHUB_REF, Some(&self.reference));
        set(&mut metadata, KEY_GITHUB_TREE_SHA, self.tree_sha.as_deref());
        set(&mut metadata, KEY_GITHUB_PATH, Some(&self.path));
        frontmatter.insert(KEY_METADATA, Value::Mapping(metadata));
    }

    /// Removes the four keys, and removes `metadata` itself if that empties it.
    pub fn clear(frontmatter: &mut SkillFrontmatter) {
        let Some(mut metadata) = frontmatter.metadata().cloned() else {
            return;
        };
        for key in PROVENANCE_KEYS {
            metadata.shift_remove(Value::String(key.to_string()));
        }
        if metadata.is_empty() {
            frontmatter.remove(KEY_METADATA);
        } else {
            frontmatter.insert(KEY_METADATA, Value::Mapping(metadata));
        }
    }
}

/// The branch assumed when provenance names no ref.
pub const DEFAULT_BRANCH: &str = "main";

/// Inserts or replaces a key, or removes it when the value is `None`.
fn set(map: &mut Mapping, key: &str, value: Option<&str>) {
    let key = Value::String(key.to_string());
    match value {
        Some(value) => {
            map.insert(key, Value::String(value.to_string()));
        }
        None => {
            map.shift_remove(&key);
        }
    }
}

/// A metadata value read as a non-empty string.
fn metadata_str(map: &Mapping, key: &str) -> Option<String> {
    let value = map.get(Value::String(key.to_string()))?.as_str()?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Splits `owner` and `repo` out of a repository URL, an `owner/repo` slug or
/// an SSH remote.
///
/// Returns `None` when either half is missing or holds a path separator.
pub fn split_owner_repo(source: &str) -> Option<(String, String)> {
    let source = source.trim().trim_end_matches('/');
    // Strip a scheme and host, or the `git@host:` prefix of an SSH remote.
    let rest = if let Some(rest) = source.split_once("://") {
        rest.1.split_once('/').map(|(_, rest)| rest)?
    } else if let Some((_, rest)) = source.split_once("git@") {
        rest.split_once(':').map(|(_, rest)| rest)?
    } else {
        source
    };
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/').filter(|p| !p.is_empty());
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

/// One entry from `~/.agents/.skill-lock.json`.
///
/// The field names mirror the lockfile's camelCase keys. Every field is
/// optional in practice: the file belongs to another tool, and a shape this
/// crate does not recognise must degrade to "no provenance" rather than to an
/// error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockEntry {
    /// The skill name the entry is filed under.
    pub name: String,
    /// The source as written, usually `owner/repo`.
    pub source: String,
    /// The kind of source, usually `github`.
    pub source_type: String,
    /// The full URL of the source.
    pub source_url: String,
    /// The skill's subdirectory within the repository.
    pub skill_path: String,
    /// The other tool's own hash of the installed folder. Its format is not
    /// documented, so it is carried and never interpreted.
    pub skill_folder_hash: String,
    /// When the skill was installed, as written.
    pub installed_at: String,
    /// When the skill was last updated, as written.
    pub updated_at: String,
}

impl LockEntry {
    /// The GitHub location this entry names, with no tree sha.
    ///
    /// Returns `None` when neither `source` nor `sourceUrl` names a repository.
    /// The result's [`Provenance::tree_sha`] is always `None`: the lockfile
    /// records no git sha, so a skill installed by `npx skills` has a known
    /// location and no baseline to compare against until Skillbase records one.
    pub fn provenance(&self) -> Option<Provenance> {
        let (owner, repo) =
            split_owner_repo(&self.source).or_else(|| split_owner_repo(&self.source_url))?;
        Some(Provenance {
            repo_url: format!("https://github.com/{owner}/{repo}"),
            reference: DEFAULT_BRANCH.to_string(),
            tree_sha: None,
            path: self.skill_path.trim_matches('/').to_string(),
        })
    }
}

/// The `npx skills` lockfile, read.
///
/// Never written. A missing, unreadable or malformed file is an empty lock, not
/// an error: this crate does not own the file and cannot repair it.
#[derive(Debug, Clone, Default)]
pub struct SkillLock {
    entries: BTreeMap<String, LockEntry>,
}

impl SkillLock {
    /// Reads `~/.agents/.skill-lock.json` under `roots`.
    pub fn read(roots: &Roots) -> SkillLock {
        Self::read_file(&Self::path(roots))
    }

    /// The lockfile's path under `roots`.
    pub fn path(roots: &Roots) -> PathBuf {
        roots.home().join(SKILL_LOCK_FILE)
    }

    /// Reads a lockfile from an explicit path.
    ///
    /// Tolerates the shapes the file has been seen in: a top-level object of
    /// name to entry, the same object under a `skills` key, and an array of
    /// entries that carry their own `name` or `skillId`. Anything else, and any
    /// entry that is not an object, is skipped.
    pub fn read_file(path: &Path) -> SkillLock {
        let Ok(text) = std::fs::read_to_string(path) else {
            return SkillLock::default();
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            return SkillLock::default();
        };

        let mut entries = BTreeMap::new();
        let root = value.get("skills").unwrap_or(&value);
        match root {
            serde_json::Value::Object(map) => {
                for (name, item) in map {
                    if let Some(entry) = parse_entry(name, item) {
                        entries.insert(entry.name.clone(), entry);
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    let name = string_field(item, &["name", "skillId", "id"]);
                    if let Some(entry) = parse_entry(&name, item) {
                        entries.insert(entry.name.clone(), entry);
                    }
                }
            }
            _ => {}
        }
        SkillLock { entries }
    }

    /// The entry filed under `name`.
    pub fn get(&self, name: &str) -> Option<&LockEntry> {
        self.entries.get(name)
    }

    /// The provenance recorded for `name`, when the lockfile names a repository
    /// for it.
    pub fn provenance(&self, name: &str) -> Option<Provenance> {
        self.get(name)?.provenance()
    }

    /// Every entry, ordered by name.
    pub fn entries(&self) -> impl Iterator<Item = &LockEntry> {
        self.entries.values()
    }

    /// How many entries were read.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when nothing was read.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Turns one JSON value into a [`LockEntry`], or `None` if it is not an object.
///
/// A top-level key that disagrees with an inner `name` loses: the key is what
/// the installed directory is called.
fn parse_entry(name: &str, item: &serde_json::Value) -> Option<LockEntry> {
    if !item.is_object() {
        return None;
    }
    let name = if name.is_empty() {
        string_field(item, &["name", "skillId", "id"])
    } else {
        name.to_string()
    };
    if name.is_empty() {
        return None;
    }
    Some(LockEntry {
        name,
        source: string_field(item, &["source"]),
        source_type: string_field(item, &["sourceType"]),
        source_url: string_field(item, &["sourceUrl"]),
        skill_path: string_field(item, &["skillPath"]),
        skill_folder_hash: string_field(item, &["skillFolderHash"]),
        installed_at: string_field(item, &["installedAt"]),
        updated_at: string_field(item, &["updatedAt"]),
    })
}

/// The first of `keys` present as a string, or the empty string.
fn string_field(item: &serde_json::Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = item.get(*key).and_then(serde_json::Value::as_str) {
            return value.to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::SkillDoc;

    fn frontmatter(yaml: &str) -> SkillFrontmatter {
        SkillFrontmatter::parse_yaml(yaml).expect("valid frontmatter")
    }

    #[test]
    fn provenance_round_trips_through_metadata() {
        let mut fm = frontmatter("name: pdf\ndescription: Reads PDFs.\n");
        let written = Provenance::new(
            "https://github.com/anthropics/skills",
            "main",
            "a1b2c3",
            "document-skills/pdf",
        );
        written.write(&mut fm);
        assert_eq!(Provenance::read(&fm), Some(written));
    }

    #[test]
    fn writing_provenance_leaves_every_other_key_alone() {
        let source = concat!(
            "---\n",
            "name: pdf\n",
            "x-agent-key: kept\n",
            "description: Reads PDFs.\n",
            "metadata:\n",
            "  version: \"1.2\"\n",
            "  author: someone\n",
            "---\n",
            "\n# PDF\n",
        );
        let mut doc = SkillDoc::parse(source).expect("parses");
        Provenance::new(
            "https://github.com/anthropics/skills",
            "v2",
            "deadbeef",
            "pdf",
        )
        .write(&mut doc.frontmatter);

        let updated = doc.to_markdown();
        let reparsed = SkillDoc::parse(&updated).expect("reparses");
        assert_eq!(
            reparsed.frontmatter.keys().collect::<Vec<_>>(),
            ["name", "x-agent-key", "description", "metadata"]
        );
        assert_eq!(
            reparsed.frontmatter.get("x-agent-key").unwrap().as_str(),
            Some("kept")
        );
        assert_eq!(reparsed.frontmatter.version().as_deref(), Some("1.2"));

        let metadata = reparsed.frontmatter.metadata().expect("metadata");
        assert_eq!(
            metadata
                .keys()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>(),
            [
                "version",
                "author",
                "github-repo",
                "github-ref",
                "github-tree-sha",
                "github-path"
            ]
        );
        // Nothing was written at the top level, where the format forbids it.
        for key in PROVENANCE_KEYS {
            assert!(
                !reparsed.frontmatter.contains_key(key),
                "{key} at top level"
            );
        }
    }

    #[test]
    fn rewriting_provenance_keeps_key_positions() {
        let mut fm = frontmatter(concat!(
            "name: pdf\n",
            "metadata:\n",
            "  github-repo: https://github.com/old/repo\n",
            "  github-ref: main\n",
            "  github-tree-sha: old\n",
            "  github-path: pdf\n",
            "  version: \"1.0\"\n",
        ));
        Provenance::new("https://github.com/new/repo", "next", "new", "docs/pdf").write(&mut fm);
        let metadata = fm.metadata().expect("metadata");
        assert_eq!(
            metadata
                .keys()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>(),
            [
                "github-repo",
                "github-ref",
                "github-tree-sha",
                "github-path",
                "version"
            ]
        );
        assert_eq!(
            Provenance::read(&fm).unwrap().tree_sha.as_deref(),
            Some("new")
        );
    }

    #[test]
    fn a_skill_with_no_provenance_reads_as_none() {
        assert_eq!(Provenance::read(&frontmatter("name: plain\n")), None);
        assert_eq!(
            Provenance::read(&frontmatter("name: plain\nmetadata:\n  version: \"1\"\n")),
            None
        );
        // A path with no repository names nothing.
        assert_eq!(
            Provenance::read(&frontmatter("metadata:\n  github-path: pdf\n")),
            None
        );
    }

    #[test]
    fn a_provenance_without_a_sha_reads_and_writes_without_one() {
        let mut fm = frontmatter("name: pdf\nmetadata:\n  github-tree-sha: stale\n");
        Provenance {
            repo_url: "https://github.com/o/r".into(),
            reference: "main".into(),
            tree_sha: None,
            path: "pdf".into(),
        }
        .write(&mut fm);
        assert!(
            !fm.metadata()
                .unwrap()
                .contains_key(Value::String(KEY_GITHUB_TREE_SHA.to_string()))
        );
        assert_eq!(Provenance::read(&fm).unwrap().tree_sha, None);
    }

    #[test]
    fn clearing_provenance_removes_an_empty_metadata_map() {
        let mut fm = frontmatter("name: pdf\n");
        Provenance::new("https://github.com/o/r", "main", "sha", "pdf").write(&mut fm);
        Provenance::clear(&mut fm);
        assert_eq!(fm.keys().collect::<Vec<_>>(), ["name"]);
    }

    #[test]
    fn owner_and_repo_come_out_of_every_source_spelling() {
        for source in [
            "https://github.com/anthropics/skills",
            "https://github.com/anthropics/skills/",
            "https://github.com/anthropics/skills.git",
            "git@github.com:anthropics/skills.git",
            "anthropics/skills",
        ] {
            assert_eq!(
                split_owner_repo(source),
                Some(("anthropics".to_string(), "skills".to_string())),
                "{source}"
            );
        }
        assert_eq!(split_owner_repo("anthropics"), None);
        assert_eq!(split_owner_repo(""), None);
    }

    fn lock_from(text: &str) -> SkillLock {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(".skill-lock.json");
        std::fs::write(&path, text).expect("write lockfile");
        SkillLock::read_file(&path)
    }

    #[test]
    fn a_lockfile_keyed_by_name_is_read() {
        let lock = lock_from(
            r#"{
              "skills": {
                "pdf": {
                  "source": "anthropics/skills",
                  "sourceType": "github",
                  "sourceUrl": "https://github.com/anthropics/skills",
                  "skillPath": "document-skills/pdf",
                  "skillFolderHash": "e3b0c442",
                  "installedAt": "2026-01-02T03:04:05Z",
                  "updatedAt": "2026-02-03T04:05:06Z"
                }
              }
            }"#,
        );
        assert_eq!(lock.len(), 1);
        let entry = lock.get("pdf").expect("entry");
        assert_eq!(entry.source_type, "github");
        assert_eq!(entry.skill_folder_hash, "e3b0c442");
        assert_eq!(entry.updated_at, "2026-02-03T04:05:06Z");

        let provenance = lock.provenance("pdf").expect("provenance");
        assert_eq!(provenance.repo_url, "https://github.com/anthropics/skills");
        assert_eq!(provenance.path, "document-skills/pdf");
        assert_eq!(provenance.tree_sha, None);
    }

    #[test]
    fn a_lockfile_without_a_skills_wrapper_is_read_too() {
        let lock = lock_from(r#"{"pdf":{"source":"o/r","skillPath":"pdf"}}"#);
        assert_eq!(lock.entries().count(), 1);
        assert_eq!(
            lock.provenance("pdf").unwrap().repo_url,
            "https://github.com/o/r"
        );
    }

    #[test]
    fn a_lockfile_holding_an_array_is_read_too() {
        let lock = lock_from(r#"{"skills":[{"name":"pdf","source":"o/r","skillPath":"pdf"}]}"#);
        assert_eq!(lock.get("pdf").unwrap().source, "o/r");
    }

    #[test]
    fn a_malformed_or_missing_lockfile_is_empty_rather_than_an_error() {
        assert!(lock_from("{ this is not json at all ").is_empty());
        assert!(lock_from("[1, 2, 3]").is_empty());
        assert!(lock_from("null").is_empty());
        assert!(lock_from(r#"{"skills":{"pdf":"a string, not an object"}}"#).is_empty());
        assert!(SkillLock::read_file(Path::new("/nonexistent/.skill-lock.json")).is_empty());
    }

    #[test]
    fn an_entry_that_names_no_repository_has_no_provenance() {
        let lock = lock_from(r#"{"skills":{"pdf":{"skillPath":"pdf"}}}"#);
        assert_eq!(lock.len(), 1);
        assert_eq!(lock.provenance("pdf"), None);
    }
}
