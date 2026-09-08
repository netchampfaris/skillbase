//! The YAML frontmatter block of a `SKILL.md`.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_yaml_ng::{Mapping, Value};

use crate::error::SkillError;

/// Key of the required skill name.
pub const KEY_NAME: &str = "name";
/// Key of the required skill description.
pub const KEY_DESCRIPTION: &str = "description";
/// Key of the optional SPDX license expression.
pub const KEY_LICENSE: &str = "license";
/// Key of the optional tool allowlist.
pub const KEY_ALLOWED_TOOLS: &str = "allowed-tools";
/// Key of the optional agent-defined metadata mapping.
pub const KEY_METADATA: &str = "metadata";
/// Key of the optional top-level version.
pub const KEY_VERSION: &str = "version";

/// The frontmatter of a skill, as an ordered mapping.
///
/// Every key is kept, in source order, including keys this crate knows nothing
/// about. `name` and `description` are not lifted into their own fields: they
/// live in the same mapping as everything else, which is what keeps their
/// position stable across a round trip. The typed accessors are views onto that
/// mapping, not a parallel copy of it.
///
/// A frontmatter parsed from text also remembers the exact source bytes and
/// replays them verbatim from [`SkillFrontmatter::to_yaml`] until something is
/// mutated. After a mutation the YAML is re-emitted, which normalizes it; see
/// the crate-level docs for exactly what changes.
#[derive(Debug, Clone, Default)]
pub struct SkillFrontmatter {
    map: Mapping,
    /// Verbatim source between the delimiters, including its trailing newline.
    /// `None` once the mapping has been mutated, or when built in memory.
    raw: Option<String>,
}

/// Two frontmatters are equal when their mappings are equal. The remembered
/// source text is a formatting detail and is not compared.
impl PartialEq for SkillFrontmatter {
    fn eq(&self, other: &Self) -> bool {
        self.map == other.map
    }
}

impl SkillFrontmatter {
    /// An empty frontmatter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Wraps an existing mapping. The result has no remembered source text, so
    /// [`SkillFrontmatter::to_yaml`] will emit freshly.
    pub fn from_mapping(map: Mapping) -> Self {
        Self { map, raw: None }
    }

    /// Parses a YAML mapping from the text between the `---` delimiters.
    ///
    /// An empty or `null` block is accepted as an empty mapping; anything else
    /// that is not a mapping is [`SkillError::FrontmatterNotMapping`].
    pub fn parse_yaml(raw: &str) -> Result<Self, SkillError> {
        let value: Value = serde_yaml_ng::from_str(raw).map_err(SkillError::MalformedYaml)?;
        let map = match value {
            Value::Null => Mapping::new(),
            Value::Mapping(map) => map,
            other => {
                return Err(SkillError::FrontmatterNotMapping {
                    found: value_kind(&other),
                });
            }
        };
        Ok(Self {
            map,
            raw: Some(raw.to_string()),
        })
    }

    /// The verbatim source text, while it is still known to match the mapping.
    pub fn raw(&self) -> Option<&str> {
        self.raw.as_deref()
    }

    /// True when the mapping has been changed since it was parsed, so
    /// [`SkillFrontmatter::to_yaml`] must re-emit rather than replay.
    pub fn is_dirty(&self) -> bool {
        self.raw.is_none()
    }

    /// The YAML block, without the `---` delimiters, or the error the
    /// serializer reported.
    ///
    /// Replays the source verbatim when nothing has been mutated. Otherwise
    /// serializes the mapping, which preserves key order but normalizes
    /// formatting. An empty mapping produces an empty string rather than `{}`.
    ///
    /// **Anything that writes a `SKILL.md` should call this rather than
    /// [`SkillFrontmatter::to_yaml`].** Serializing a mapping fails only for a
    /// value YAML cannot spell — a value nested inside two tags, say — but when
    /// it does, the alternative to an error is a file whose frontmatter is
    /// shorter than the one that was read, and no error is worse than a saved
    /// skill with no `name` in it.
    ///
    /// Skillbase's own interface cannot produce such a value: it edits
    /// frontmatter through the string accessors only. The guard is for callers
    /// of [`SkillFrontmatter::as_mapping_mut`], which hands out the mapping
    /// itself and so admits any `Value` at all.
    pub fn try_to_yaml(&self) -> Result<String, serde_yaml_ng::Error> {
        if let Some(raw) = &self.raw {
            return Ok(raw.clone());
        }
        if self.map.is_empty() {
            return Ok(String::new());
        }
        serde_yaml_ng::to_string(&self.map)
    }

    /// The YAML block, without the `---` delimiters, on the assumption that it
    /// can be produced.
    ///
    /// The same as [`SkillFrontmatter::try_to_yaml`] except that a serializer
    /// error is answered with the entries that *can* be serialized instead of
    /// being reported. That is a last resort for a caller with nowhere to put
    /// an error, and it still drops the offending entry, so a caller that is
    /// about to overwrite a file on disk must use `try_to_yaml` and refuse the
    /// write instead.
    pub fn to_yaml(&self) -> String {
        self.try_to_yaml().unwrap_or_else(|_| self.to_yaml_lossy())
    }

    /// Every entry that can be serialized, in order, skipping the ones that
    /// cannot.
    ///
    /// Entries are added one at a time and the growing mapping is re-serialized
    /// after each, so one unspellable value costs its own key and no other. It
    /// is only reachable from [`SkillFrontmatter::to_yaml`], where the previous
    /// behaviour was to return an empty string and take `name` and
    /// `description` down with the offending key.
    fn to_yaml_lossy(&self) -> String {
        let mut kept = Mapping::new();
        for (key, value) in &self.map {
            let mut probe = kept.clone();
            probe.insert(key.clone(), value.clone());
            if serde_yaml_ng::to_string(&probe).is_ok() {
                kept = probe;
            }
        }
        if kept.is_empty() {
            return String::new();
        }
        serde_yaml_ng::to_string(&kept).unwrap_or_default()
    }

    /// Forgets the source text, so the next [`SkillFrontmatter::to_yaml`]
    /// re-emits the mapping. Useful to normalize a file deliberately.
    pub fn normalize(&mut self) {
        self.raw = None;
    }

    /// The underlying mapping.
    pub fn as_mapping(&self) -> &Mapping {
        &self.map
    }

    /// The underlying mapping, mutably. Taking this reference marks the
    /// frontmatter dirty, because the mapping may change through it.
    pub fn as_mapping_mut(&mut self) -> &mut Mapping {
        self.raw = None;
        &mut self.map
    }

    /// Consumes the frontmatter and returns its mapping.
    pub fn into_mapping(self) -> Mapping {
        self.map
    }

    /// Keys in source order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.map.keys().filter_map(Value::as_str)
    }

    /// Every key other than `name` and `description`, in source order.
    pub fn extra(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.map.iter().filter_map(|(k, v)| {
            let k = k.as_str()?;
            (k != KEY_NAME && k != KEY_DESCRIPTION).then_some((k, v))
        })
    }

    /// Looks up a raw value.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.map.get(Value::String(key.to_string()))
    }

    /// True when the key is present.
    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Inserts or replaces a value, returning the old one.
    ///
    /// Replacing an existing key keeps its position, so the order of unknown
    /// agent-specific keys survives an edit.
    pub fn insert(&mut self, key: impl Into<String>, value: Value) -> Option<Value> {
        self.raw = None;
        self.map.insert(Value::String(key.into()), value)
    }

    /// Removes a key, returning its value.
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        self.raw = None;
        // `Mapping::remove` is a *swap*-remove, which would move the last key
        // into the hole. `shift_remove` keeps the surviving keys in order.
        self.map.shift_remove(Value::String(key.to_string()))
    }

    /// Number of top-level keys.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// True when there are no keys at all.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The required `name`, when present and a string.
    pub fn name(&self) -> Option<&str> {
        self.get(KEY_NAME).and_then(Value::as_str)
    }

    /// Sets `name`, keeping its position if it already exists.
    pub fn set_name(&mut self, name: impl Into<String>) {
        self.insert(KEY_NAME, Value::String(name.into()));
    }

    /// The required `description`, when present and a string.
    pub fn description(&self) -> Option<&str> {
        self.get(KEY_DESCRIPTION).and_then(Value::as_str)
    }

    /// Sets `description`, keeping its position if it already exists.
    pub fn set_description(&mut self, description: impl Into<String>) {
        self.insert(KEY_DESCRIPTION, Value::String(description.into()));
    }

    /// The optional `license`, when present and a string.
    pub fn license(&self) -> Option<&str> {
        self.get(KEY_LICENSE).and_then(Value::as_str)
    }

    /// The optional `allowed-tools` list.
    ///
    /// Agents write this either as a comma-separated string
    /// (`allowed-tools: Read, Grep`) or as a YAML sequence. Both are accepted
    /// and normalized to a list of trimmed names; empty entries are dropped.
    pub fn allowed_tools(&self) -> Option<Vec<String>> {
        match self.get(KEY_ALLOWED_TOOLS)? {
            Value::String(s) => Some(
                s.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect(),
            ),
            Value::Sequence(items) => Some(
                items
                    .iter()
                    .filter_map(scalar_to_string)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            ),
            _ => None,
        }
    }

    /// The optional `metadata` mapping.
    pub fn metadata(&self) -> Option<&Mapping> {
        self.get(KEY_METADATA).and_then(Value::as_mapping)
    }

    /// The skill version: top-level `version` if present, otherwise
    /// `metadata.version`. Numbers are rendered as strings, because YAML would
    /// otherwise turn `version: 1.10` into `1.1`.
    pub fn version(&self) -> Option<String> {
        if let Some(v) = self.get(KEY_VERSION).and_then(scalar_to_string) {
            return Some(v);
        }
        self.metadata()?
            .get(Value::String(KEY_VERSION.to_string()))
            .and_then(scalar_to_string)
    }
}

/// Renders a YAML scalar as a string. Non-scalars return `None`.
fn scalar_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// A human-readable name for a YAML value's kind, for error messages.
fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Sequence(_) => "a sequence",
        Value::Mapping(_) => "a mapping",
        Value::Tagged(_) => "a tagged value",
    }
}

impl Serialize for SkillFrontmatter {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.map.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SkillFrontmatter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Mapping::deserialize(deserializer).map(Self::from_mapping)
    }
}

#[cfg(test)]
mod tests {
    use serde_yaml_ng::value::{Tag, TaggedValue};

    use super::*;

    fn parse(yaml: &str) -> SkillFrontmatter {
        SkillFrontmatter::parse_yaml(yaml).expect("valid frontmatter")
    }

    #[test]
    fn typed_accessors_read_the_common_optional_keys() {
        let fm = parse(concat!(
            "name: my-skill\n",
            "description: Does a thing.\n",
            "license: Apache-2.0\n",
            "allowed-tools: Read, Grep, Bash\n",
            "metadata:\n",
            "  version: \"1.0\"\n",
        ));
        assert_eq!(fm.name(), Some("my-skill"));
        assert_eq!(fm.description(), Some("Does a thing."));
        assert_eq!(fm.license(), Some("Apache-2.0"));
        assert_eq!(
            fm.allowed_tools().unwrap(),
            vec!["Read".to_string(), "Grep".to_string(), "Bash".to_string()]
        );
        assert!(fm.metadata().is_some());
        assert_eq!(fm.version().as_deref(), Some("1.0"));
    }

    #[test]
    fn allowed_tools_accepts_a_sequence_too() {
        let fm = parse("allowed-tools:\n  - Read\n  - Bash(git status:*)\n");
        assert_eq!(
            fm.allowed_tools().unwrap(),
            vec!["Read".to_string(), "Bash(git status:*)".to_string()]
        );
    }

    #[test]
    fn top_level_version_wins_over_metadata_version() {
        let fm = parse("version: 2\nmetadata:\n  version: \"1.0\"\n");
        assert_eq!(fm.version().as_deref(), Some("2"));
    }

    #[test]
    fn keys_and_extra_report_source_order() {
        let fm = parse("zeta: 1\nname: a\nalpha: 2\ndescription: b\n");
        assert_eq!(
            fm.keys().collect::<Vec<_>>(),
            ["zeta", "name", "alpha", "description"]
        );
        assert_eq!(
            fm.extra().map(|(k, _)| k).collect::<Vec<_>>(),
            ["zeta", "alpha"]
        );
    }

    #[test]
    fn replacing_a_key_keeps_its_position() {
        let mut fm = parse("zeta: 1\nname: a\nalpha: 2\n");
        fm.set_name("b");
        assert_eq!(fm.keys().collect::<Vec<_>>(), ["zeta", "name", "alpha"]);
        assert_eq!(fm.name(), Some("b"));
    }

    #[test]
    fn removing_a_key_keeps_the_order_of_the_rest() {
        let mut fm = parse("a: 1\nb: 2\nc: 3\n");
        fm.remove("b");
        assert_eq!(fm.keys().collect::<Vec<_>>(), ["a", "c"]);
    }

    #[test]
    fn to_yaml_replays_the_source_until_a_mutation() {
        let source = "name:   my-skill\n# a comment\ndescription: 'Does a thing.'\n";
        let mut fm = parse(source);
        assert!(!fm.is_dirty());
        assert_eq!(fm.to_yaml(), source);

        fm.set_name("other-skill");
        assert!(fm.is_dirty());
        assert_ne!(fm.to_yaml(), source);
        assert!(fm.to_yaml().contains("other-skill"));
    }

    #[test]
    fn a_null_or_empty_block_is_an_empty_mapping() {
        assert!(parse("").is_empty());
        assert!(parse("\n").is_empty());
        assert!(parse("null\n").is_empty());
    }

    #[test]
    fn a_non_mapping_block_is_rejected() {
        let err = SkillFrontmatter::parse_yaml("- one\n- two\n").unwrap_err();
        assert!(matches!(
            err,
            SkillError::FrontmatterNotMapping {
                found: "a sequence"
            }
        ));
    }

    #[test]
    fn malformed_yaml_is_rejected() {
        let err = SkillFrontmatter::parse_yaml("name: [unterminated\n").unwrap_err();
        assert!(matches!(err, SkillError::MalformedYaml(_)));
    }

    #[test]
    fn an_empty_in_memory_mapping_emits_nothing() {
        assert_eq!(SkillFrontmatter::new().to_yaml(), "");
        assert_eq!(SkillFrontmatter::new().try_to_yaml().unwrap(), "");
    }

    /// A value inside two tags. YAML has no spelling for it, so the serializer
    /// refuses the whole mapping — the one realistic way this can happen.
    fn doubly_tagged() -> Value {
        let inner = Value::Tagged(Box::new(TaggedValue {
            tag: Tag::new("Inner"),
            value: Value::String("x".into()),
        }));
        Value::Tagged(Box::new(TaggedValue {
            tag: Tag::new("Outer"),
            value: inner,
        }))
    }

    #[test]
    fn try_to_yaml_reports_what_the_serializer_refuses() {
        let mut fm = parse("name: pdf\ndescription: Reads PDFs.\n");
        fm.as_mapping_mut()
            .insert(Value::String("metadata".into()), doubly_tagged());

        assert!(
            fm.try_to_yaml().is_err(),
            "a doubly tagged value must not serialize"
        );
    }

    #[test]
    fn to_yaml_keeps_the_rest_when_one_value_cannot_be_serialized() {
        let mut fm = parse("name: pdf\ndescription: Reads PDFs.\n");
        fm.as_mapping_mut()
            .insert(Value::String("metadata".into()), doubly_tagged());

        // The old behaviour was an empty string here, which `to_markdown` then
        // wrote out as `---\n---\n`: a skill with no name and no description.
        let yaml = fm.to_yaml();
        assert!(yaml.contains("name: pdf"), "{yaml:?}");
        assert!(yaml.contains("description: Reads PDFs."), "{yaml:?}");
        assert!(!yaml.contains("metadata"), "{yaml:?}");
    }

    #[test]
    fn to_yaml_matches_try_to_yaml_when_nothing_is_wrong() {
        let mut fm = parse("name: pdf\ndescription: Reads PDFs.\n");
        fm.set_name("pdf-reader");
        assert_eq!(fm.to_yaml(), fm.try_to_yaml().unwrap());
        assert!(fm.to_yaml().contains("pdf-reader"));
    }
}
