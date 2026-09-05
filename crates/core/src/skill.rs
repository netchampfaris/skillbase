//! A skill as it exists on disk: a directory with a `SKILL.md` in it.

use std::fs;
use std::path::{Path, PathBuf};

use crate::doc::SkillDoc;
use crate::error::{SkillError, ValidationIssue};

/// The file every skill directory must contain.
pub const SKILL_FILE_NAME: &str = "SKILL.md";

/// Directory names skipped when listing bundled files.
const IGNORED_DIRS: &[&str] = &[".git"];

/// A skill directory: its `SKILL.md` and the files bundled alongside it.
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    /// The skill directory, not the `SKILL.md` inside it.
    pub path: PathBuf,
    /// The parsed `SKILL.md`.
    pub doc: SkillDoc,
    bundled_files: Vec<PathBuf>,
}

impl Skill {
    /// Builds a skill in memory, with no bundled files.
    pub fn new(path: impl Into<PathBuf>, doc: SkillDoc) -> Self {
        Self {
            path: path.into(),
            doc,
            bundled_files: Vec::new(),
        }
    }

    /// Reads `dir/SKILL.md` and lists the files bundled with it.
    ///
    /// Fails only if the file cannot be read or is not structurally a
    /// frontmatter document. A skill that breaks the format still loads; call
    /// [`Skill::validate`] to find out how.
    pub fn load(dir: &Path) -> Result<Self, SkillError> {
        let file = dir.join(SKILL_FILE_NAME);
        let text = fs::read_to_string(&file).map_err(|e| SkillError::io(&file, e))?;
        let doc = SkillDoc::parse(&text)?;
        Ok(Self {
            path: dir.to_path_buf(),
            doc,
            bundled_files: list_bundled_files(dir)?,
        })
    }

    /// Writes `SKILL.md` back, creating the directory if needed.
    ///
    /// Bundled files are only ever listed, never written or deleted, so saving
    /// cannot lose them.
    pub fn save(&self) -> Result<(), SkillError> {
        fs::create_dir_all(&self.path).map_err(|e| SkillError::io(&self.path, e))?;
        let file = self.skill_md_path();
        fs::write(&file, self.doc.to_markdown()).map_err(|e| SkillError::io(&file, e))
    }

    /// Path of the `SKILL.md` itself.
    pub fn skill_md_path(&self) -> PathBuf {
        self.path.join(SKILL_FILE_NAME)
    }

    /// Bundled files, as paths relative to the skill directory, sorted, with
    /// `SKILL.md` itself excluded.
    pub fn bundled_files(&self) -> &[PathBuf] {
        &self.bundled_files
    }

    /// Re-reads the bundled file listing from disk.
    pub fn refresh_bundled_files(&mut self) -> Result<(), SkillError> {
        self.bundled_files = list_bundled_files(&self.path)?;
        Ok(())
    }

    /// The directory's own name, which conventionally matches the skill name.
    pub fn dir_name(&self) -> Option<&str> {
        self.path.file_name()?.to_str()
    }

    /// The document's own issues, plus a warning when the directory name and
    /// the frontmatter `name` disagree.
    pub fn validate(&self) -> Vec<ValidationIssue> {
        let mut issues = self.doc.validate();
        if let (Some(dir), Some(name)) = (self.dir_name(), self.doc.frontmatter.name())
            && dir != name
        {
            issues.push(ValidationIssue::warning(
                Some("name"),
                SkillError::NameDirectoryMismatch {
                    dir: dir.to_string(),
                    name: name.to_string(),
                },
            ));
        }
        issues
    }

    /// True when neither the document nor the directory raises an error.
    pub fn is_valid(&self) -> bool {
        !self.validate().iter().any(ValidationIssue::is_error)
    }
}

/// Walks `dir` and returns every file below it, relative and sorted, without
/// `SKILL.md` and without anything under an ignored directory.
fn list_bundled_files(dir: &Path) -> Result<Vec<PathBuf>, SkillError> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = fs::read_dir(&current).map_err(|e| SkillError::io(&current, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| SkillError::io(&current, e))?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|e| SkillError::io(&path, e))?;
            let name = entry.file_name();
            if file_type.is_dir() {
                if !IGNORED_DIRS.iter().any(|ignored| name == *ignored) {
                    stack.push(path);
                }
            } else {
                let relative = path.strip_prefix(dir).unwrap_or(&path).to_path_buf();
                if relative != Path::new(SKILL_FILE_NAME) {
                    files.push(relative);
                }
            }
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A temp directory that deletes itself. Avoids a dev-dependency.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "skillbase-core-{label}-{}-{nanos}-{n}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const SOURCE: &str = concat!(
        "---\n",
        "name: pdf-tools\n",
        "x-agent-private: keep me\n",
        "description: Reads and writes PDFs.\n",
        "allowed-tools: Read, Bash\n",
        "---\n",
        "\n",
        "# PDF Tools\n",
        "\n",
        "Run `scripts/extract.py`.\n",
    );

    fn skill_dir() -> TempDir {
        let temp = TempDir::new("load");
        let dir = temp.path().join("pdf-tools");
        fs::create_dir_all(&dir).unwrap();
        temp.write("pdf-tools/SKILL.md", SOURCE);
        temp.write("pdf-tools/scripts/extract.py", "print('hi')\n");
        temp.write("pdf-tools/scripts/lib/helper.py", "# helper\n");
        temp.write("pdf-tools/references/spec.md", "# Spec\n");
        temp.write("pdf-tools/assets/logo.svg", "<svg/>\n");
        temp.write("pdf-tools/.git/config", "[core]\n");
        temp
    }

    #[test]
    fn load_reads_the_document_and_lists_bundled_files() {
        let temp = skill_dir();
        let skill = Skill::load(&temp.path().join("pdf-tools")).unwrap();

        assert_eq!(skill.doc.frontmatter.name(), Some("pdf-tools"));
        assert_eq!(skill.dir_name(), Some("pdf-tools"));
        assert_eq!(skill.doc.to_markdown(), SOURCE);
        assert!(skill.is_valid());

        let files: Vec<_> = skill
            .bundled_files()
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(
            files,
            [
                "assets/logo.svg",
                "references/spec.md",
                "scripts/extract.py",
                "scripts/lib/helper.py",
            ],
            "sorted, relative, no SKILL.md and nothing from .git"
        );
    }

    #[test]
    fn save_rewrites_only_skill_md_and_leaves_bundled_files_alone() {
        let temp = skill_dir();
        let dir = temp.path().join("pdf-tools");
        let mut skill = Skill::load(&dir).unwrap();

        skill.doc.frontmatter.set_description("Now edited.");
        skill.save().unwrap();

        let reloaded = Skill::load(&dir).unwrap();
        assert_eq!(reloaded.doc.frontmatter.description(), Some("Now edited."));
        assert_eq!(
            reloaded.doc.frontmatter.keys().collect::<Vec<_>>(),
            ["name", "x-agent-private", "description", "allowed-tools"],
            "an edit through save must not reorder or drop keys"
        );
        assert_eq!(
            reloaded.doc.body,
            "\n# PDF Tools\n\nRun `scripts/extract.py`.\n"
        );
        assert_eq!(reloaded.bundled_files(), skill.bundled_files());
        assert!(temp.path().join("pdf-tools/scripts/extract.py").exists());
    }

    #[test]
    fn save_without_edits_leaves_the_file_byte_identical() {
        let temp = skill_dir();
        let dir = temp.path().join("pdf-tools");
        Skill::load(&dir).unwrap().save().unwrap();
        assert_eq!(
            fs::read_to_string(dir.join(SKILL_FILE_NAME)).unwrap(),
            SOURCE
        );
    }

    #[test]
    fn save_creates_a_missing_directory() {
        let temp = TempDir::new("create");
        let dir = temp.path().join("new-skill");
        let doc = SkillDoc::parse("---\nname: new-skill\ndescription: d\n---\n# Hi\n").unwrap();
        let skill = Skill::new(&dir, doc);
        skill.save().unwrap();
        assert!(dir.join(SKILL_FILE_NAME).is_file());
        assert_eq!(
            Skill::load(&dir).unwrap().bundled_files(),
            &[] as &[PathBuf]
        );
    }

    #[test]
    fn refresh_picks_up_a_new_bundled_file() {
        let temp = skill_dir();
        let dir = temp.path().join("pdf-tools");
        let mut skill = Skill::load(&dir).unwrap();
        assert_eq!(skill.bundled_files().len(), 4);
        temp.write("pdf-tools/scripts/new.py", "\n");
        skill.refresh_bundled_files().unwrap();
        assert_eq!(skill.bundled_files().len(), 5);
    }

    #[test]
    fn a_missing_skill_md_is_an_io_error_naming_the_path() {
        let temp = TempDir::new("missing");
        let err = Skill::load(temp.path()).unwrap_err();
        let SkillError::Io { path, .. } = &err else {
            panic!("expected an io error, got {err:?}");
        };
        assert!(path.ends_with(SKILL_FILE_NAME));
        assert!(err.to_string().contains(SKILL_FILE_NAME));
    }

    #[test]
    fn a_directory_name_that_differs_from_the_skill_name_is_a_warning() {
        let temp = TempDir::new("mismatch");
        let dir = temp.path().join("wrong-dir");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(SKILL_FILE_NAME),
            "---\nname: pdf-tools\ndescription: d\n---\n# Hi\n",
        )
        .unwrap();
        let skill = Skill::load(&dir).unwrap();
        let issues = skill.validate();
        assert_eq!(issues.len(), 1);
        assert!(matches!(
            issues[0].error,
            SkillError::NameDirectoryMismatch { .. }
        ));
        assert!(skill.is_valid(), "a mismatch is a warning, not an error");
    }
}
