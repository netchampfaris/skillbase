//! Errors, validation issues and the format limits they refer to.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum length of a skill `name`, in characters.
pub const MAX_NAME_LEN: usize = 64;

/// Maximum length of a skill `description`, in characters.
pub const MAX_DESCRIPTION_LEN: usize = 1024;

/// Anything that can go wrong while reading, validating or writing a skill.
///
/// The enum is shared by two very different code paths, on purpose:
///
/// * [`SkillDoc::parse`](crate::SkillDoc::parse) returns only the *hard* variants,
///   the ones that mean "this text is not a SKILL.md at all":
///   [`MissingFrontmatter`](Self::MissingFrontmatter),
///   [`UnterminatedFrontmatter`](Self::UnterminatedFrontmatter),
///   [`MalformedYaml`](Self::MalformedYaml) and
///   [`FrontmatterNotMapping`](Self::FrontmatterNotMapping).
/// * [`SkillDoc::validate`](crate::SkillDoc::validate) reports the *soft* variants
///   as [`ValidationIssue`]s. They describe a skill that parsed fine but does not
///   satisfy the format, so an editor can open it, show it as invalid and let the
///   user fix it in place.
///
/// See the crate-level docs for why the split is drawn there.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SkillError {
    /// The text does not begin with a `---` delimiter line.
    #[error("no YAML frontmatter: the file must begin with a line containing only `---`")]
    MissingFrontmatter,

    /// The opening `---` was never closed by a `---` or `...` line.
    #[error("unterminated YAML frontmatter: no closing `---` line was found")]
    UnterminatedFrontmatter,

    /// The frontmatter block is not valid YAML.
    #[error("malformed YAML frontmatter: {0}")]
    MalformedYaml(#[source] serde_yaml_ng::Error),

    /// The frontmatter parsed, but as something other than a mapping.
    #[error("YAML frontmatter must be a mapping, found {found}")]
    FrontmatterNotMapping {
        /// The YAML kind that was found instead, e.g. `"sequence"`.
        found: &'static str,
    },

    /// A field that must hold a string holds something else.
    #[error("frontmatter field `{field}` must be a string")]
    NotAString {
        /// The offending key.
        field: String,
    },

    /// The required `name` field is absent or empty.
    #[error("frontmatter is missing the required `name` field")]
    MissingName,

    /// The required `description` field is absent or empty.
    #[error("frontmatter is missing the required `description` field")]
    MissingDescription,

    /// `name` is not kebab-case.
    #[error("skill name `{name}` is not kebab-case (expected /^[a-z0-9]+(-[a-z0-9]+)*$/)")]
    NameNotKebabCase {
        /// The offending name.
        name: String,
    },

    /// `name` is longer than [`MAX_NAME_LEN`].
    #[error("skill name is {len} characters, the maximum is {max}")]
    NameTooLong {
        /// Actual length in characters.
        len: usize,
        /// The limit, [`MAX_NAME_LEN`].
        max: usize,
    },

    /// `description` is longer than [`MAX_DESCRIPTION_LEN`].
    #[error("skill description is {len} characters, the maximum is {max}")]
    DescriptionTooLong {
        /// Actual length in characters.
        len: usize,
        /// The limit, [`MAX_DESCRIPTION_LEN`].
        max: usize,
    },

    /// The Markdown body below the frontmatter is blank.
    #[error("skill body is empty")]
    EmptyBody,

    /// The directory name and the frontmatter `name` disagree.
    #[error("directory name `{dir}` does not match the frontmatter name `{name}`")]
    NameDirectoryMismatch {
        /// The directory's file name.
        dir: String,
        /// The `name` in the frontmatter.
        name: String,
    },

    /// A filesystem operation failed.
    #[error("{path}: {source}")]
    Io {
        /// The path being read or written.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

impl SkillError {
    /// Builds an [`SkillError::Io`] carrying the path that failed.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// How much a [`ValidationIssue`] matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// The skill violates the format. Agents may refuse to load it.
    Error,
    /// The skill loads, but something looks wrong.
    Warning,
}

/// One finding from [`SkillDoc::validate`](crate::SkillDoc::validate).
#[derive(Debug)]
pub struct ValidationIssue {
    /// How much this finding matters.
    pub severity: Severity,
    /// The frontmatter key it concerns, when it concerns one.
    pub field: Option<&'static str>,
    /// What is wrong.
    pub error: SkillError,
}

impl ValidationIssue {
    /// A finding that makes the skill invalid.
    pub fn error(field: Option<&'static str>, error: SkillError) -> Self {
        Self {
            severity: Severity::Error,
            field,
            error,
        }
    }

    /// A finding worth surfacing that still leaves the skill loadable.
    pub fn warning(field: Option<&'static str>, error: SkillError) -> Self {
        Self {
            severity: Severity::Warning,
            field,
            error,
        }
    }

    /// True when this issue makes the skill invalid.
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl std::fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.severity {
            Severity::Error => write!(f, "error: {}", self.error),
            Severity::Warning => write!(f, "warning: {}", self.error),
        }
    }
}
