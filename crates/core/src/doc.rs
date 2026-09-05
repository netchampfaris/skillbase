//! The parsed contents of a `SKILL.md`: frontmatter plus Markdown body.

use crate::error::{MAX_DESCRIPTION_LEN, MAX_NAME_LEN, Severity, SkillError, ValidationIssue};
use crate::frontmatter::{KEY_DESCRIPTION, KEY_NAME, SkillFrontmatter};
use crate::slug::is_kebab_case;

/// Byte-order mark, which some editors put in front of the first `---`.
const BOM: char = '\u{feff}';

/// Which newline a document uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LineEnding {
    /// `\n`.
    #[default]
    Lf,
    /// `\r\n`.
    Crlf,
}

impl LineEnding {
    /// The newline itself.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        }
    }

    /// The newline used by the first line break in `text`, defaulting to LF.
    pub fn detect(text: &str) -> Self {
        match text.find('\n') {
            Some(i) if text[..i].ends_with('\r') => Self::Crlf,
            _ => Self::Lf,
        }
    }
}

/// A parsed `SKILL.md`.
///
/// # Round-trip
///
/// `SkillDoc::parse(text)?.to_markdown() == text`, byte for byte, for any text
/// this crate can parse — including CRLF, a byte-order mark, a missing trailing
/// newline, comments in the frontmatter, and whatever quoting or block-scalar
/// style the author used. That holds because the document keeps the delimiter
/// lines, the frontmatter source and the body as verbatim strings, and only
/// re-emits YAML once the frontmatter is actually mutated.
///
/// After a mutation the frontmatter is serialized again. Key order still
/// survives, but formatting is normalized; [`SkillDoc::to_markdown`] documents
/// exactly what changes.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillDoc {
    /// The YAML frontmatter.
    pub frontmatter: SkillFrontmatter,
    /// Everything after the closing delimiter line, verbatim.
    pub body: String,
    /// The newline to use when re-emitting frontmatter.
    line_ending: LineEnding,
    /// The opening delimiter line, with any BOM and its newline.
    open_delim: String,
    /// The closing delimiter line, with its newline if it had one.
    close_delim: String,
}

impl SkillDoc {
    /// Builds a document in memory. Emits LF and `---` delimiters.
    pub fn new(frontmatter: SkillFrontmatter, body: impl Into<String>) -> Self {
        Self {
            frontmatter,
            body: body.into(),
            line_ending: LineEnding::Lf,
            open_delim: "---\n".to_string(),
            close_delim: "---\n".to_string(),
        }
    }

    /// Parses the text of a `SKILL.md`.
    ///
    /// Parsing is deliberately permissive. It fails only when the text is not
    /// structurally a frontmatter document — no opening `---`, no closing
    /// delimiter, or a frontmatter block that is not a YAML mapping. It does
    /// *not* fail on a missing `name`, a name in the wrong case, an
    /// over-long description or any other format violation: those come back
    /// from [`SkillDoc::validate`] so that an editor can load the file, show it
    /// as invalid and let the user repair it in place. A parse error means
    /// there is nothing to show; a validation error means there is something to
    /// fix.
    pub fn parse(text: &str) -> Result<Self, SkillError> {
        let line_ending = LineEnding::detect(text);

        // The opening delimiter must be the very first line. A `---` anywhere
        // else in the file is ordinary Markdown: a horizontal rule, a line
        // inside a fenced code block, a YAML document separator in an example.
        let (first_line, after_first) = split_line(text);
        let bom_len = first_line.starts_with(BOM).then(|| BOM.len_utf8());
        let delimiter = &first_line[bom_len.unwrap_or(0)..];
        if !is_delimiter(delimiter, false) {
            return Err(SkillError::MissingFrontmatter);
        }
        // A bare `---` with no newline after it cannot be a closed frontmatter.
        let Some(after_first) = after_first else {
            return Err(SkillError::UnterminatedFrontmatter);
        };
        let open_delim = text[..text.len() - after_first.len()].to_string();

        // Scan forward for the first line that closes the block. Everything up
        // to it is YAML, everything after it is the body, verbatim.
        let mut rest = after_first;
        let mut yaml_len = 0usize;
        loop {
            let (line, after) = split_line(rest);
            if is_delimiter(line, true) {
                let close_len = rest.len() - after.map_or(0, str::len);
                let yaml = after_first[..yaml_len].to_string();
                let close_delim = rest[..close_len].to_string();
                let body = after.unwrap_or("").to_string();
                return Ok(Self {
                    frontmatter: SkillFrontmatter::parse_yaml(&yaml)?,
                    body,
                    line_ending,
                    open_delim,
                    close_delim,
                });
            }
            let Some(after) = after else {
                return Err(SkillError::UnterminatedFrontmatter);
            };
            yaml_len += rest.len() - after.len();
            rest = after;
        }
    }

    /// Renders the document back to `SKILL.md` text.
    ///
    /// While the frontmatter is untouched this reproduces the parsed source
    /// byte for byte. Once the frontmatter has been mutated the YAML block is
    /// re-emitted by `serde_yaml_ng`, and the following are normalized:
    ///
    /// * comments and blank lines inside the frontmatter are dropped, because
    ///   the YAML data model does not carry them;
    /// * quoting is re-chosen — `'single'` and unnecessary `"double"` quotes
    ///   become plain scalars, and a string that would otherwise read as
    ///   another type gains quotes (the string `007` is emitted as `'007'`);
    /// * block-scalar *style* is re-chosen: a multi-line string is emitted as a
    ///   literal block scalar (`|`, `|-`) whatever style it was written in, and
    ///   a folded scalar (`>-`) whose value has no newline left in it becomes a
    ///   single-line scalar. The string value is unchanged, only its spelling;
    /// * indentation, spacing after `:` and flow/block style are normalized;
    /// * anchors and aliases are expanded; merge keys are resolved.
    ///
    /// Key order is *not* normalized. `Mapping` is backed by an `IndexMap`, so
    /// unknown agent-specific keys keep their source positions, and replacing a
    /// value in place keeps that key's position too.
    ///
    /// The body is never touched, in either case.
    pub fn to_markdown(&self) -> String {
        let mut yaml = self.frontmatter.to_yaml();
        if self.frontmatter.is_dirty() && self.line_ending == LineEnding::Crlf {
            // A freshly serialized block is always LF; match the document.
            yaml = yaml.replace('\n', "\r\n");
        }
        let mut out = String::with_capacity(
            self.open_delim.len() + yaml.len() + self.close_delim.len() + self.body.len(),
        );
        out.push_str(&self.open_delim);
        out.push_str(&yaml);
        out.push_str(&self.close_delim);
        out.push_str(&self.body);
        out
    }

    /// The newline this document uses.
    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    /// Switches the newline used when re-emitting frontmatter.
    pub fn set_line_ending(&mut self, line_ending: LineEnding) {
        self.line_ending = line_ending;
    }

    /// Checks the skill against the format rules.
    ///
    /// This is separate from [`SkillDoc::parse`] on purpose: a file that is
    /// merely wrong should still open in an editor. Errors mean an agent may
    /// refuse to load the skill; warnings are worth showing but harmless.
    ///
    /// Errors: `name` or `description` missing, empty or not a string; `name`
    /// not kebab-case; `name` over [`MAX_NAME_LEN`]; `description` over
    /// [`MAX_DESCRIPTION_LEN`]. Warning: a blank Markdown body.
    pub fn validate(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();

        match self.frontmatter.get(KEY_NAME) {
            None => issues.push(ValidationIssue::error(
                Some(KEY_NAME),
                SkillError::MissingName,
            )),
            Some(value) if value.as_str().is_none() => issues.push(ValidationIssue::error(
                Some(KEY_NAME),
                SkillError::NotAString {
                    field: KEY_NAME.to_string(),
                },
            )),
            Some(_) => {
                let name = self.frontmatter.name().unwrap_or_default();
                if name.trim().is_empty() {
                    issues.push(ValidationIssue::error(
                        Some(KEY_NAME),
                        SkillError::MissingName,
                    ));
                } else {
                    let len = name.chars().count();
                    if len > MAX_NAME_LEN {
                        issues.push(ValidationIssue::error(
                            Some(KEY_NAME),
                            SkillError::NameTooLong {
                                len,
                                max: MAX_NAME_LEN,
                            },
                        ));
                    }
                    if !is_kebab_case(name) {
                        issues.push(ValidationIssue::error(
                            Some(KEY_NAME),
                            SkillError::NameNotKebabCase {
                                name: name.to_string(),
                            },
                        ));
                    }
                }
            }
        }

        match self.frontmatter.get(KEY_DESCRIPTION) {
            None => issues.push(ValidationIssue::error(
                Some(KEY_DESCRIPTION),
                SkillError::MissingDescription,
            )),
            Some(value) if value.as_str().is_none() => issues.push(ValidationIssue::error(
                Some(KEY_DESCRIPTION),
                SkillError::NotAString {
                    field: KEY_DESCRIPTION.to_string(),
                },
            )),
            Some(_) => {
                let description = self.frontmatter.description().unwrap_or_default();
                if description.trim().is_empty() {
                    issues.push(ValidationIssue::error(
                        Some(KEY_DESCRIPTION),
                        SkillError::MissingDescription,
                    ));
                } else {
                    let len = description.chars().count();
                    if len > MAX_DESCRIPTION_LEN {
                        issues.push(ValidationIssue::error(
                            Some(KEY_DESCRIPTION),
                            SkillError::DescriptionTooLong {
                                len,
                                max: MAX_DESCRIPTION_LEN,
                            },
                        ));
                    }
                }
            }
        }

        if self.body.trim().is_empty() {
            issues.push(ValidationIssue::warning(None, SkillError::EmptyBody));
        }

        issues
    }

    /// True when [`SkillDoc::validate`] finds no error-severity issues.
    pub fn is_valid(&self) -> bool {
        !self
            .validate()
            .iter()
            .any(|issue| issue.severity == Severity::Error)
    }
}

/// Splits off the first line. Returns the line without its newline, and the
/// remainder after the newline, or `None` when the line was not terminated.
fn split_line(text: &str) -> (&str, Option<&str>) {
    match text.find('\n') {
        Some(i) => (
            text[..i].strip_suffix('\r').unwrap_or(&text[..i]),
            Some(&text[i + 1..]),
        ),
        None => (text.strip_suffix('\r').unwrap_or(text), None),
    }
}

/// True when a line is a frontmatter delimiter. Trailing spaces are tolerated,
/// as YAML tolerates them. `...` closes a YAML document, so it is accepted as a
/// closing delimiter only.
fn is_delimiter(line: &str, closing: bool) -> bool {
    let line = line.trim_end();
    line == "---" || (closing && line == "...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml_ng::Value;

    const EXAMPLE: &str = concat!(
        "---\n",
        "name: my-skill\n",
        "description: When and what this skill does.\n",
        "license: Apache-2.0\n",
        "allowed-tools: Read, Grep, Bash\n",
        "metadata:\n",
        "  version: \"1.0\"\n",
        "---\n",
        "\n",
        "# My Skill\n",
        "\n",
        "Markdown body...\n",
    );

    #[test]
    fn parses_the_example_document() {
        let doc = SkillDoc::parse(EXAMPLE).unwrap();
        assert_eq!(doc.frontmatter.name(), Some("my-skill"));
        assert_eq!(
            doc.frontmatter.description(),
            Some("When and what this skill does.")
        );
        assert_eq!(doc.frontmatter.license(), Some("Apache-2.0"));
        assert_eq!(
            doc.frontmatter.allowed_tools().unwrap(),
            ["Read", "Grep", "Bash"]
        );
        assert_eq!(doc.frontmatter.version().as_deref(), Some("1.0"));
        assert_eq!(doc.body, "\n# My Skill\n\nMarkdown body...\n");
        assert_eq!(doc.line_ending(), LineEnding::Lf);
        assert!(doc.is_valid());
        assert_eq!(doc.to_markdown(), EXAMPLE);
    }

    /// Every one of these must survive `parse` -> `to_markdown` byte for byte.
    #[test]
    fn round_trips_byte_for_byte() {
        let cases: &[(&str, &str)] = &[
            ("the example", EXAMPLE),
            (
                "unknown agent-specific keys",
                "---\nname: a\nx-agent-thing: {deep: [1, 2]}\ndescription: b\nzz_last: true\n---\nbody\n",
            ),
            (
                "comments and odd spacing",
                "---\n# top comment\nname:    a   # trailing\n\ndescription: >-\n  folded\n  text\n---\nbody\n",
            ),
            ("empty body", "---\nname: a\ndescription: b\n---\n"),
            (
                "no body and no trailing newline",
                "---\nname: a\ndescription: b\n---",
            ),
            (
                "body without a trailing newline",
                "---\nname: a\ndescription: b\n---\nno newline",
            ),
            ("empty frontmatter", "---\n---\n# body\n"),
            (
                "crlf",
                "---\r\nname: a\r\ndescription: b\r\n---\r\n\r\n# Body\r\n",
            ),
            (
                "byte-order mark",
                "\u{feff}---\nname: a\ndescription: b\n---\nbody\n",
            ),
            (
                "closed with dots",
                "---\nname: a\ndescription: b\n...\nbody\n",
            ),
            (
                "single quotes and escapes",
                "---\nname: 'a'\ndescription: \"line\\nbreak\"\n---\nbody\n",
            ),
            (
                "unicode",
                "---\nname: unicode-skill\ndescription: \"Résumé 日本語 — em dash, emoji 🚀\"\n---\n\n# Ünïcödé\n",
            ),
        ];
        for (label, text) in cases {
            let doc = SkillDoc::parse(text).unwrap_or_else(|e| panic!("{label}: {e}"));
            assert_eq!(&doc.to_markdown(), text, "{label} did not round-trip");
        }
    }

    #[test]
    fn round_trip_preserves_unknown_keys_and_their_order() {
        let text = concat!(
            "---\n",
            "zzz-agent-private: keep me\n",
            "name: my-skill\n",
            "argument-hint: <file>\n",
            "description: Does a thing.\n",
            "model: opus\n",
            "nested:\n",
            "  a: 1\n",
            "  b:\n",
            "    - x\n",
            "    - y\n",
            "---\n",
            "body\n",
        );
        let doc = SkillDoc::parse(text).unwrap();
        assert_eq!(
            doc.frontmatter.keys().collect::<Vec<_>>(),
            [
                "zzz-agent-private",
                "name",
                "argument-hint",
                "description",
                "model",
                "nested"
            ]
        );
        assert_eq!(doc.to_markdown(), text);
    }

    #[test]
    fn editing_one_field_keeps_every_other_key_and_its_order() {
        let text = concat!(
            "---\n",
            "zzz-agent-private: keep me\n",
            "name: old-name\n",
            "argument-hint: <file>\n",
            "description: Does a thing.\n",
            "nested:\n",
            "  a: 1\n",
            "  b:\n",
            "    - x\n",
            "    - y\n",
            "---\n",
            "\n# Body stays exactly as it was\n\n---\n",
        );
        let mut doc = SkillDoc::parse(text).unwrap();
        doc.frontmatter.set_name("new-name");
        let out = doc.to_markdown();

        let reparsed = SkillDoc::parse(&out).unwrap();
        assert_eq!(reparsed.frontmatter.name(), Some("new-name"));
        assert_eq!(
            reparsed.frontmatter.keys().collect::<Vec<_>>(),
            [
                "zzz-agent-private",
                "name",
                "argument-hint",
                "description",
                "nested"
            ],
            "unknown keys must not be dropped or reordered"
        );
        // Every value other than `name` is untouched, nested ones included.
        assert_eq!(
            reparsed.frontmatter.get("zzz-agent-private"),
            Some(&Value::String("keep me".into()))
        );
        assert_eq!(
            reparsed.frontmatter.get("nested"),
            doc.frontmatter.get("nested")
        );
        // The Markdown body is never re-rendered.
        assert_eq!(reparsed.body, "\n# Body stays exactly as it was\n\n---\n");
    }

    /// The one thing that does not survive an *edit*: YAML spelling. The values
    /// are identical, the text is not. Nothing here is a body change.
    #[test]
    fn re_emitting_normalizes_yaml_spelling_but_not_values() {
        let text = concat!(
            "---\n",
            "# this comment is lost on re-emit\n",
            "name:    'quoted-name'\n",
            "\n",
            "description: |-\n",
            "  first line\n",
            "  second line\n",
            "summary: >-\n",
            "  folded onto\n",
            "  one line\n",
            "count: 007\n",
            "---\n",
            "body\n",
        );
        let doc = SkillDoc::parse(text).unwrap();
        assert_eq!(
            doc.to_markdown(),
            text,
            "untouched documents replay exactly"
        );

        let mut edited = doc.clone();
        edited.frontmatter.normalize(); // force re-emission, changing nothing
        let out = edited.to_markdown();

        assert_eq!(
            out,
            concat!(
                "---\n",
                // the comment and the blank line are gone, the quotes dropped
                "name: quoted-name\n",
                // a multi-line string stays a block scalar
                "description: |-\n",
                "  first line\n",
                "  second line\n",
                // `>-` folded away the newline, so this is single-line now
                "summary: folded onto one line\n",
                // a string that would otherwise read as a number gains quotes
                "count: '007'\n",
                "---\n",
                "body\n",
            ),
            "the exact normalization is asserted so a change in it is visible"
        );

        // Semantically nothing moved.
        let reparsed = SkillDoc::parse(&out).unwrap();
        assert_eq!(reparsed.frontmatter, doc.frontmatter);
        assert_eq!(reparsed.body, doc.body);
        assert_eq!(
            reparsed.frontmatter.description(),
            Some("first line\nsecond line")
        );
    }

    #[test]
    fn crlf_is_preserved_when_the_frontmatter_is_re_emitted() {
        let text = "---\r\nname: a\r\ndescription: b\r\n---\r\nbody\r\n";
        let mut doc = SkillDoc::parse(text).unwrap();
        assert_eq!(doc.line_ending(), LineEnding::Crlf);
        doc.frontmatter.set_name("c");
        let out = doc.to_markdown();
        assert_eq!(out, "---\r\nname: c\r\ndescription: b\r\n---\r\nbody\r\n");
        assert!(!out.contains("\n\r"));
    }

    #[test]
    fn an_empty_body_parses_and_warns() {
        let doc = SkillDoc::parse("---\nname: a\ndescription: b\n---\n").unwrap();
        assert_eq!(doc.body, "");
        let issues = doc.validate();
        assert_eq!(issues.len(), 1);
        assert!(matches!(issues[0].error, SkillError::EmptyBody));
        assert_eq!(issues[0].severity, Severity::Warning);
        assert!(doc.is_valid(), "a blank body is a warning, not an error");
    }

    #[test]
    fn a_file_with_no_frontmatter_is_rejected() {
        for text in [
            "# Just Markdown\n\nNo frontmatter here.\n",
            "",
            "\n---\nname: a\n---\n",
            "not a delimiter\n---\nname: a\n---\n",
            "--\nname: a\n--\n",
            "----\nname: a\n----\n",
        ] {
            assert!(
                matches!(SkillDoc::parse(text), Err(SkillError::MissingFrontmatter)),
                "should have no frontmatter: {text:?}"
            );
        }
    }

    #[test]
    fn an_unterminated_frontmatter_is_rejected() {
        for text in ["---", "---\n", "---\nname: a\ndescription: b\n"] {
            assert!(
                matches!(
                    SkillDoc::parse(text),
                    Err(SkillError::UnterminatedFrontmatter)
                ),
                "should be unterminated: {text:?}"
            );
        }
    }

    #[test]
    fn dashes_in_the_body_are_not_the_terminator() {
        let text = concat!(
            "---\n",
            "name: my-skill\n",
            "description: Uses --- in the body.\n",
            "---\n",
            "# Title\n",
            "\n",
            "---\n", // a horizontal rule
            "\n",
            "Some prose.\n",
            "\n",
            "```yaml\n",
            "---\n", // a document separator inside a fence
            "key: value\n",
            "---\n",
            "```\n",
            "\n",
            "...\n", // a bare ellipsis line
            "\n",
            "Setext heading\n",
            "---\n",
            "\n",
            "Last line.\n",
        );
        let doc = SkillDoc::parse(text).unwrap();
        assert_eq!(doc.frontmatter.len(), 2);
        assert_eq!(doc.frontmatter.name(), Some("my-skill"));
        assert!(doc.body.starts_with("# Title\n"));
        assert!(doc.body.ends_with("Last line.\n"));
        assert_eq!(doc.body.matches("\n---\n").count(), 4);
        assert_eq!(doc.to_markdown(), text);
    }

    #[test]
    fn a_yaml_code_fence_in_the_body_is_left_alone() {
        let text = concat!(
            "---\n",
            "name: yaml-demo\n",
            "description: Shows a YAML fence.\n",
            "---\n",
            "# Example\n",
            "\n",
            "```yaml\n",
            "name: not-the-real-name\n",
            "description: not the real description\n",
            "nested:\n",
            "  - a\n",
            "```\n",
        );
        let doc = SkillDoc::parse(text).unwrap();
        assert_eq!(doc.frontmatter.name(), Some("yaml-demo"));
        assert!(doc.body.contains("name: not-the-real-name"));
        assert_eq!(doc.to_markdown(), text);
    }

    #[test]
    fn block_scalar_descriptions_are_read_as_plain_strings() {
        let folded = SkillDoc::parse(concat!(
            "---\n",
            "name: folded\n",
            "description: >-\n",
            "  Use this skill when the user asks about invoices.\n",
            "  It reads the ledger and writes a summary.\n",
            "---\n",
            "body\n",
        ))
        .unwrap();
        assert_eq!(
            folded.frontmatter.description(),
            Some(
                "Use this skill when the user asks about invoices. It reads the ledger and writes a summary."
            ),
            "`>-` folds newlines into spaces and strips the trailing one"
        );

        let literal = SkillDoc::parse(concat!(
            "---\n",
            "name: literal\n",
            "description: |\n",
            "  First line.\n",
            "  Second line.\n",
            "---\n",
            "body\n",
        ))
        .unwrap();
        assert_eq!(
            literal.frontmatter.description(),
            Some("First line.\nSecond line.\n"),
            "`|` keeps newlines and the final one"
        );
        assert!(literal.is_valid());
    }

    #[test]
    fn unicode_in_the_description_is_preserved_and_counted_in_characters() {
        let text = concat!(
            "---\n",
            "name: unicode-skill\n",
            "description: Résumé helper — 日本語, emoji 🚀, and “smart quotes”.\n",
            "---\n",
            "body\n",
        );
        let doc = SkillDoc::parse(text).unwrap();
        let description = doc.frontmatter.description().unwrap();
        assert!(description.contains('🚀'));
        assert!(description.contains("日本語"));
        assert!(doc.is_valid());
        assert_eq!(doc.to_markdown(), text);

        // Over-length is measured in characters, not bytes: 600 four-byte
        // emoji are 2400 bytes but only 600 characters.
        let long = "🚀".repeat(600);
        let doc = SkillDoc::parse(&format!(
            "---\nname: a\ndescription: \"{long}\"\n---\nbody\n"
        ))
        .unwrap();
        assert!(doc.is_valid(), "600 characters is under the 1024 limit");
    }

    #[test]
    fn malformed_frontmatter_yaml_is_rejected() {
        let err = SkillDoc::parse("---\nname: [oops\n---\nbody\n").unwrap_err();
        assert!(matches!(err, SkillError::MalformedYaml(_)));
    }

    #[test]
    fn a_non_mapping_frontmatter_is_rejected() {
        let err = SkillDoc::parse("---\n- a\n- b\n---\nbody\n").unwrap_err();
        assert!(matches!(
            err,
            SkillError::FrontmatterNotMapping {
                found: "a sequence"
            }
        ));
    }

    fn issues_of(frontmatter: &str) -> Vec<ValidationIssue> {
        SkillDoc::parse(&format!("---\n{frontmatter}---\nbody\n"))
            .unwrap()
            .validate()
    }

    #[test]
    fn validation_flags_bad_names() {
        assert!(matches!(
            issues_of("name: My-Skill\ndescription: d\n")[0].error,
            SkillError::NameNotKebabCase { .. }
        ));
        assert!(matches!(
            issues_of("name: my skill\ndescription: d\n")[0].error,
            SkillError::NameNotKebabCase { .. }
        ));
        assert!(matches!(
            issues_of("name: my_skill\ndescription: d\n")[0].error,
            SkillError::NameNotKebabCase { .. }
        ));
        assert!(matches!(
            issues_of("name: -leading\ndescription: d\n")[0].error,
            SkillError::NameNotKebabCase { .. }
        ));
        assert!(matches!(
            issues_of("description: d\n")[0].error,
            SkillError::MissingName
        ));
        assert!(matches!(
            issues_of("name: ''\ndescription: d\n")[0].error,
            SkillError::MissingName
        ));
        assert!(matches!(
            issues_of("name: 42\ndescription: d\n")[0].error,
            SkillError::NotAString { .. }
        ));
    }

    #[test]
    fn validation_flags_an_over_long_name() {
        let name = "a".repeat(MAX_NAME_LEN + 1);
        let issues = issues_of(&format!("name: {name}\ndescription: d\n"));
        assert!(matches!(
            issues[0].error,
            SkillError::NameTooLong { len: 65, max: 64 }
        ));
        // Exactly at the limit is fine.
        let name = "a".repeat(MAX_NAME_LEN);
        assert!(issues_of(&format!("name: {name}\ndescription: d\n")).is_empty());
    }

    #[test]
    fn validation_flags_a_missing_or_over_long_description() {
        assert!(matches!(
            issues_of("name: a\n")[0].error,
            SkillError::MissingDescription
        ));
        assert!(matches!(
            issues_of("name: a\ndescription: '   '\n")[0].error,
            SkillError::MissingDescription
        ));
        assert!(matches!(
            issues_of("name: a\ndescription: []\n")[0].error,
            SkillError::NotAString { .. }
        ));

        let description = "d".repeat(MAX_DESCRIPTION_LEN + 1);
        assert!(matches!(
            issues_of(&format!("name: a\ndescription: {description}\n"))[0].error,
            SkillError::DescriptionTooLong {
                len: 1025,
                max: 1024
            }
        ));
        let description = "d".repeat(MAX_DESCRIPTION_LEN);
        assert!(issues_of(&format!("name: a\ndescription: {description}\n")).is_empty());
    }

    #[test]
    fn an_invalid_skill_still_parses_so_it_can_be_edited() {
        let text = "---\nname: Not Kebab\n---\n";
        let doc = SkillDoc::parse(text).expect("parse must not fail on a soft violation");
        assert!(!doc.is_valid());
        assert_eq!(doc.validate().len(), 3); // bad name, no description, empty body
        assert_eq!(doc.to_markdown(), text);
    }

    #[test]
    fn a_document_built_in_memory_renders_correctly() {
        let mut frontmatter = SkillFrontmatter::new();
        frontmatter.set_name("built-here");
        frontmatter.set_description("Made in memory.");
        let doc = SkillDoc::new(frontmatter, "# Body\n");
        assert_eq!(
            doc.to_markdown(),
            "---\nname: built-here\ndescription: Made in memory.\n---\n# Body\n"
        );
        assert!(doc.is_valid());
    }
}
