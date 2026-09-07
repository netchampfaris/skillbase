//! A fake home directory for the discovery and install tests.
//!
//! Every test in this crate that touches the filesystem runs against a
//! [`Fixture`], never the real home. [`Fixture::realistic`] reproduces the
//! shapes a machine actually has: skills whose origin is the store, which is
//! the shared directory `~/.agents/skills`, one of them symlinked into Claude
//! Code as well; a managed skill hidden from the agents, with its origin in the
//! private directory and a link from Claude Code; an unmanaged skill owned by
//! Claude Code; a copy fan-out into another agent; a link parked in
//! `skills-disabled`; a broken link; a directory without a `SKILL.md`; a skill
//! whose frontmatter disagrees with its directory name; and a Codex config that
//! switches one skill off.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::discovery::{Discovery, DiscoveryResult};
use crate::install::Installer;
use crate::registry::{Registry, Roots};

/// A temporary home directory and the roots resolved against it.
pub struct Fixture {
    _temp: TempDir,
    home: PathBuf,
}

impl Fixture {
    /// A home with nothing in it.
    pub fn empty() -> Self {
        let temp = TempDir::new().expect("temp dir");
        // macOS reaches a temp directory through /var, a symlink to
        // /private/var. Canonicalizing here keeps fixture paths comparable to
        // the ones `fs::canonicalize` hands back during a scan.
        let home = fs::canonicalize(temp.path()).expect("canonical temp dir");
        Self { _temp: temp, home }
    }

    /// A home laid out the way a real machine is.
    pub fn realistic() -> Self {
        let fx = Self::empty();

        // Skills whose origin is the store, which is the shared directory. Every
        // agent that reads `~/.agents/skills` reaches them with no link.
        fx.skill(".agents/skills/shared-one", "shared-one");
        fx.skill(".agents/skills/shared-two", "shared-two");
        fx.skill(".agents/skills/copied-around", "copied-around");
        fx.skill(".agents/skills/disabled-here", "disabled-here");
        // A directory name that disagrees with the frontmatter name.
        fx.skill(".agents/skills/wrong-dir-name", "renamed-inside");
        // A SKILL.md that is not a frontmatter document at all.
        fx.dir(".agents/skills/broken-doc");
        fx.write_file(".agents/skills/broken-doc/SKILL.md", "# No frontmatter\n");
        // Things in a skills directory that are not skills.
        fx.dir(".agents/skills/not-a-skill");
        fx.write_file(".agents/skills/not-a-skill/README.md", "nothing here\n");
        fx.dir(".agents/skills/skill-md-is-a-dir/SKILL.md");
        fx.write_file(".agents/skills/.DS_Store", "\u{0}\n");
        symlink(
            fx.home.join(".agents/skills/nowhere-at-all"),
            fx.home.join(".agents/skills/dangling"),
        )
        .expect("dangling symlink");

        // A managed skill hidden from the agents that read the shared
        // directory: its origin sits in the private directory, and only the
        // link Claude Code holds reaches it.
        fx.skill(".skillbase/private/hidden-one", "hidden-one");
        fx.link_rel(
            ".claude/skills/hidden-one",
            "../../.skillbase/private/hidden-one",
        );

        // Claude Code: one link into the store, one skill of its own.
        fx.dir(".claude/skills");
        fx.link_rel(
            ".claude/skills/shared-one",
            "../../.agents/skills/shared-one",
        );
        fx.skill(".claude/skills/claude-only", "claude-only");
        // A link parked in the disabled directory.
        fx.link_rel(
            ".claude/skills-disabled/disabled-here",
            "../../.agents/skills/disabled-here",
        );

        // A copy fan-out: the same skill, twice, in two real directories.
        fx.skill(".gemini/skills/copied-around", "copied-around");

        // An agent directory that exists but holds nothing.
        fx.dir(".cursor/skills");

        // Codex has switched one skill off without moving it.
        fx.write_file(
            ".codex/config.toml",
            &format!(
                "# a comment Skillbase must not eat\n\
                 [tui]\n\
                 theme = \"dark\"\n\n\
                 [[skills.config]]\n\
                 path = \"{}/.agents/skills/shared-one/SKILL.md\"\n\
                 enabled = false\n",
                fx.home.display()
            ),
        );

        // Something precious outside every scope, for the guard tests.
        fx.dir("Documents/secret");
        fx.write_file("Documents/secret/keep-me.txt", "do not delete me\n");

        fx
    }

    /// The temporary home directory, canonicalized.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Roots resolved against this home.
    pub fn roots(&self) -> Roots {
        Roots::new(&self.home)
    }

    /// An installer confined to this home.
    pub fn installer(&self) -> Installer {
        Installer::new(self.roots())
    }

    /// Scans this home.
    pub fn scan(&self) -> DiscoveryResult {
        Discovery::new(self.roots()).run()
    }

    /// `~/.agents/skills`, seen as the directory agents read.
    pub fn shared(&self) -> PathBuf {
        self.roots().shared_dir()
    }

    /// `~/.agents/skills`, seen as the directory Skillbase owns the bytes in.
    /// The same path as [`Fixture::shared`].
    pub fn store(&self) -> PathBuf {
        self.roots().store_dir()
    }

    /// `~/.skillbase/private`.
    pub fn private(&self) -> PathBuf {
        self.roots().private_dir()
    }

    /// One agent's global skills directory.
    pub fn agent(&self, id: &str) -> PathBuf {
        self.roots()
            .agent_dir(Registry::get(id).expect("an agent in the table"))
    }

    /// Creates a directory, and its parents.
    pub fn dir(&self, relative: &str) -> PathBuf {
        let path = self.home.join(relative);
        fs::create_dir_all(&path).expect("create dir");
        path
    }

    /// Writes a file, creating its parents.
    pub fn write_file(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.home.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(&path, contents).expect("write file");
        path
    }

    /// Creates a skill directory whose frontmatter carries `name`.
    pub fn skill(&self, relative: &str, name: &str) -> PathBuf {
        let dir = self.dir(relative);
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: The {name} skill.\n---\n\n# {name}\n"),
        )
        .expect("write SKILL.md");
        dir
    }

    /// Creates a skill that carries the provenance `gh skill install` and
    /// Skillbase both write: the four `github-*` keys, nested under `metadata`.
    ///
    /// `source` is an `owner/repo` slug, `path` the skill's subdirectory in
    /// that repository.
    pub fn installed_skill(
        &self,
        relative: &str,
        name: &str,
        source: &str,
        path: &str,
        tree_sha: &str,
    ) -> PathBuf {
        let dir = self.dir(relative);
        fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\n\
                 name: {name}\n\
                 description: The {name} skill.\n\
                 metadata:\n  \
                   github-repo: https://github.com/{source}\n  \
                   github-ref: main\n  \
                   github-tree-sha: {tree_sha}\n  \
                   github-path: {path}\n\
                 ---\n\n# {name}\n"
            ),
        )
        .expect("write SKILL.md");
        dir
    }

    /// Writes the lockfile `npx skills` maintains, with one entry per
    /// `(name, source, skill path)`.
    ///
    /// Skillbase reads this file and never writes it, so the fixture is what
    /// stands in for the other tool having been run.
    pub fn skill_lock(&self, entries: &[(&str, &str, &str)]) -> PathBuf {
        let rows: Vec<String> = entries
            .iter()
            .map(|(name, source, path)| {
                format!(
                    "\"{name}\":{{\
                       \"source\":\"{source}\",\
                       \"sourceType\":\"github\",\
                       \"sourceUrl\":\"https://github.com/{source}\",\
                       \"skillPath\":\"{path}\",\
                       \"skillFolderHash\":\"e3b0c44298fc\",\
                       \"installedAt\":\"2026-01-02T03:04:05Z\",\
                       \"updatedAt\":\"2026-01-02T03:04:05Z\"}}"
                )
            })
            .collect();
        self.write_file(
            crate::provenance::SKILL_LOCK_FILE,
            &format!("{{\"skills\":{{{}}}}}", rows.join(",")),
        )
    }

    /// Creates a symlink at `relative` pointing at `target`, as written.
    pub fn link_rel(&self, relative: &str, target: &str) -> PathBuf {
        let path = self.home.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        symlink(target, &path).expect("symlink");
        path
    }

    /// Creates a symlink in the shared directory pointing at an absolute path.
    pub fn symlink_raw(&self, name: &str, target: &Path) {
        symlink(target, self.shared().join(name)).expect("symlink");
    }
}

/// A full recursive listing of `root`: every path, what it is, and what it
/// holds.
///
/// Symlinks are recorded by their target as written and never followed, so two
/// listings compare equal only when the tree is genuinely unchanged. Used to
/// prove that discovery writes nothing and that a refused operation changed
/// nothing.
pub fn listing(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .display()
                .to_string();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                out.push(format!("{relative}\tunreadable"));
                continue;
            };
            if meta.file_type().is_symlink() {
                let target = fs::read_link(&path).unwrap_or_default();
                out.push(format!("{relative}\tlink -> {}", target.display()));
            } else if meta.is_dir() {
                out.push(format!("{relative}\tdir"));
                stack.push(path);
            } else {
                let bytes = fs::read(&path).unwrap_or_default();
                out.push(format!("{relative}\tfile {} {:?}", bytes.len(), bytes));
            }
        }
    }
    out.sort();
    out
}
