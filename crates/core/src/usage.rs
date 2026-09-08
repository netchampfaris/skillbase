//! How often each skill has actually been invoked.
//!
//! # Two agents out of fifteen record this at all
//!
//! This is the honest statement the rest of the module is built around. Of the
//! agents in [`Registry::all`], exactly two leave a machine-readable record of a
//! skill being used:
//!
//! * **Claude Code**, in its session transcripts under `~/.claude/projects/`.
//! * **GitHub Copilot CLI**, in its event log under `~/.copilot/session-state/`.
//!
//! Codex, Cursor, Gemini CLI, opencode, Goose, Amp, Zed, Cline, Junie, Warp,
//! Kiro and Devin keep no such record. Not a sparse one, not one in a different
//! shape: none. A count of zero for a skill installed only into those agents
//! means "not measurable", not "not used", and the interface has to say so.
//!
//! There is a tempting heuristic for Codex — its shell history holds
//! `cat …/SKILL.md` calls — and this module deliberately does not use it. That
//! counts *file reads*, which happen during grep sweeps, editing and this very
//! application's own scans, and it over-counts real use by roughly ten times. A
//! number that wrong is worse than no number.
//!
//! # The counts are a rolling window
//!
//! Claude Code prunes transcripts at about [`CLAUDE_RETENTION_DAYS`] days, so
//! these are "uses in the last month or so", never lifetime totals. Counts
//! legitimately go *down* between two loads. [`Usage::sources`] reports which
//! sources were read and how many files each contributed, so the interface can
//! qualify the number instead of presenting it as a fact about all time.
//!
//! # What is counted, per source
//!
//! Claude Code writes one JSON object per line. Two separate, non-overlapping
//! signals mean a skill ran:
//!
//! 1. **Model-invoked.** An `assistant` row whose `message.content` array holds
//!    a `tool_use` block named `Skill`, with the skill in `input.skill`.
//! 2. **User-invoked.** A `user` row whose `message.content` is a plain
//!    *string* holding a slash-command envelope. A slash command emits no
//!    `Skill` tool call at all, so counting only (1) badly undercounts.
//!
//! `toolUseResult.commandName` is ignored: it restates (1).
//!
//! Copilot is simpler: a `skill.invoked` event carries `data.name`. Its
//! `data.path` names the origin directory too, which this module does not
//! record — resolving a name to a directory is [`crate::discovery`]'s job, and
//! it already does it better.
//!
//! # Performance
//!
//! A real machine holds hundreds of megabytes of transcript across a few
//! hundred files, and fewer than one line in four hundred is about a skill.
//! Handing every line to `serde_json` is far too slow, so each line is first
//! tested for a cheap byte substring and only the survivors are parsed. A cache
//! at [`USAGE_CACHE_FILE`] then makes the *next* load read only the bytes each
//! transcript grew by.
//!
//! Both are blocking file IO. [`Usage::load`] is meant for a background task.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::registry::Roots;

/// Skillbase's usage cache, relative to the home directory.
///
/// Inside `~/.skillbase/`, because nothing in this application writes anywhere
/// else.
pub const USAGE_CACHE_FILE: &str = ".skillbase/usage.json";

/// Claude Code's session transcripts, relative to the home directory.
///
/// One directory per project, holding one `.jsonl` file per session. The
/// project directories are flat: transcripts are exactly one level down.
pub const CLAUDE_TRANSCRIPT_DIR: &str = ".claude/projects";

/// GitHub Copilot CLI's session state, relative to the home directory.
///
/// One directory per session, each holding an `events.jsonl`.
pub const COPILOT_SESSION_DIR: &str = ".copilot/session-state";

/// Roughly how long Claude Code keeps a transcript before pruning it.
///
/// The reason counts are a rolling window rather than a lifetime total.
pub const CLAUDE_RETENTION_DAYS: u32 = 30;

/// The agent ids that record skill invocations, in registry order.
///
/// Compare against [`Registry::all`] to say "2 of 15" honestly.
///
/// [`Registry::all`]: crate::registry::Registry::all
pub const RECORDING_AGENT_IDS: &[&str] = &["claude-code", "copilot"];

/// Bumped whenever [`FileRecord`] changes shape. An older or newer cache is
/// discarded rather than migrated.
const CACHE_VERSION: u32 = 1;

/// Read buffer per transcript. Large enough that a 300 MB file is a few
/// thousand syscalls, small enough to stay off the stack of concerns.
const READ_BUFFER: usize = 128 * 1024;

/// An agent that records skill invocations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UsageSource {
    /// Claude Code's session transcripts.
    ClaudeCode,
    /// GitHub Copilot CLI's session events.
    Copilot,
}

impl UsageSource {
    /// Every source, in registry order.
    pub const ALL: [UsageSource; 2] = [Self::ClaudeCode, Self::Copilot];

    /// The agent id in [`Registry`](crate::registry::Registry).
    pub fn agent_id(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Copilot => "copilot",
        }
    }

    /// Name to show a human.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Copilot => "GitHub Copilot",
        }
    }

    /// The directory its session records live in, under `roots`.
    pub fn dir(self, roots: &Roots) -> PathBuf {
        match self {
            Self::ClaudeCode => roots.home().join(CLAUDE_TRANSCRIPT_DIR),
            Self::Copilot => roots.home().join(COPILOT_SESSION_DIR),
        }
    }

    /// The session files, which both sources keep one directory level down.
    ///
    /// A missing root means the agent is not installed, which is the common
    /// case and not a warning. Sorted, so a scan is deterministic.
    fn files(self, roots: &Roots, warnings: &mut Vec<String>) -> Vec<PathBuf> {
        let root = self.dir(roots);
        let mut files = Vec::new();
        for dir in list_dir(&root, warnings) {
            if !dir.is_dir() {
                continue;
            }
            match self {
                Self::ClaudeCode => files.extend(
                    list_dir(&dir, warnings)
                        .into_iter()
                        .filter(|p| p.extension().is_some_and(|e| e == "jsonl")),
                ),
                Self::Copilot => {
                    let events = dir.join("events.jsonl");
                    if events.is_file() {
                        files.push(events);
                    }
                }
            }
        }
        files.sort();
        files
    }

    /// The prefilter: could this line possibly be about a skill?
    ///
    /// This is the whole performance story. On a real machine roughly 330 of
    /// 160,000 transcript lines survive it, and only those reach `serde_json`.
    /// It runs on raw bytes so that the other 99.8% are never even validated as
    /// UTF-8.
    ///
    /// Each needle is anchored on a byte that is *rare in JSON* rather than on
    /// its first one: anchoring `"Skill"` on the opening quote would restart the
    /// search at every one of the dozens of quotes in a transcript row.
    fn may_mention_a_skill(self, line: &[u8]) -> bool {
        match self {
            // `"Skill"` catches the tool_use block whatever the spacing;
            // `<command-message>` catches the slash-command envelope.
            Self::ClaudeCode => {
                has_bytes(line, b"\"Skill\"", 1) || has_bytes(line, b"<command-message>", 0)
            }
            Self::Copilot => has_bytes(line, b"skill.invoked", 5),
        }
    }

    /// Adds the skill names one line invokes to `counts`, as recorded — plugin
    /// namespace and all. `seen` deduplicates rows by uuid within one file.
    fn count_line(
        self,
        line: &str,
        seen: &mut HashSet<String>,
        counts: &mut BTreeMap<String, u32>,
    ) {
        match self {
            Self::ClaudeCode => count_transcript_line(line, seen, counts),
            Self::Copilot => count_copilot_line(line, counts),
        }
    }
}

/// What one source contributed to a set of counts.
///
/// Lets the interface say "from 152 Claude Code transcripts" rather than
/// presenting a bare number as if it covered the whole machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceStat {
    /// The agent whose records these are.
    pub source: UsageSource,
    /// How many session files were accounted for, whether read fresh or reused
    /// from the cache.
    pub files: usize,
    /// Total invocations counted from this source.
    pub invocations: u32,
}

/// Invocation counts per skill name, and what they were counted from.
///
/// Build one with [`Usage::load`]. The default value is the "nothing has been
/// loaded yet" state: empty, with no sources, so [`Usage::is_empty`] is true.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    /// Keyed by the bare skill name, with any plugin namespace stripped, so it
    /// matches the name [`crate::discovery`] reports.
    counts: HashMap<String, u32>,
    /// The plugins a name was seen under, when it was namespaced.
    plugins: BTreeMap<String, Vec<String>>,
    sources: Vec<SourceStat>,
    warnings: Vec<String>,
}

impl Usage {
    /// Counts every invocation recorded under `roots`.
    ///
    /// **Blocking.** Reads up to a few hundred megabytes on a first load, and
    /// only the bytes transcripts have grown by on later ones. Call it from a
    /// background task.
    ///
    /// Never fails. An unreadable directory or file becomes a warning and the
    /// scan carries on, exactly as in [`crate::discovery`]. A cache that cannot
    /// be *read* is not even that: it silently means "do a full scan", and the
    /// scan that follows answers the same question. A cache that cannot be
    /// *written* is a warning, because it means every later load repeats the
    /// full scan and nothing else on the machine will ever say why.
    pub fn load(roots: &Roots) -> Usage {
        let mut warnings = Vec::new();
        let cache_path = roots.home().join(USAGE_CACHE_FILE);
        let cached = Cache::read(&cache_path);

        let mut fresh: BTreeMap<String, FileRecord> = BTreeMap::new();
        let mut raw: BTreeMap<String, u32> = BTreeMap::new();
        let mut sources = Vec::new();

        for source in UsageSource::ALL {
            let mut files = 0usize;
            let mut invocations = 0u32;
            for path in source.files(roots, &mut warnings) {
                let key = path.to_string_lossy().into_owned();
                let Some(record) = load_file(source, &path, cached.files.get(&key), &mut warnings)
                else {
                    continue;
                };
                files += 1;
                for (name, n) in &record.counts {
                    *raw.entry(name.clone()).or_default() += n;
                    invocations += n;
                }
                fresh.insert(key, record);
            }
            // A source with files but no invocations is still data: it means
            // "used nothing", which is different from "recorded nothing".
            if files > 0 {
                sources.push(SourceStat {
                    source,
                    files,
                    invocations,
                });
            }
        }

        // Files that vanished are simply absent from `fresh`. Claude Code prunes
        // at ~30 days, so this is routine and the counts decay with it.
        if let Err(warning) = Cache::write(&cache_path, fresh) {
            warnings.push(warning);
        }

        let mut counts: HashMap<String, u32> = HashMap::new();
        let mut plugins: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (recorded, n) in raw {
            let (plugin, name) = split_namespace(&recorded);
            if let Some(plugin) = plugin {
                let seen = plugins.entry(name.to_string()).or_default();
                if !seen.iter().any(|p| p == plugin) {
                    seen.push(plugin.to_string());
                }
            }
            *counts.entry(name.to_string()).or_default() += n;
        }

        Usage {
            counts,
            plugins,
            sources,
            warnings,
        }
    }

    /// How many times this skill was invoked. Zero when it was never seen.
    ///
    /// Zero is only meaningful when [`Usage::is_empty`] is false; see that
    /// method for the distinction the interface has to draw.
    pub fn count(&self, skill: &str) -> u32 {
        self.counts.get(skill).copied().unwrap_or(0)
    }

    /// True when no source could be read at all.
    ///
    /// The interface needs this to tell "no usage data on this machine" apart
    /// from "a column of honest zeros". Both look identical through
    /// [`Usage::count`], and roughly three skills in five on a typical machine
    /// genuinely score zero, so the two are easy to confuse.
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Warnings, in the same shape [`crate::discovery`] uses: one line each,
    /// in the order they were noticed.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// What each source contributed, in registry order. Only sources that had
    /// at least one session file appear.
    pub fn sources(&self) -> &[SourceStat] {
        &self.sources
    }

    /// True when this source's records were read.
    pub fn has_source(&self, source: UsageSource) -> bool {
        self.sources.iter().any(|s| s.source == source)
    }

    /// Every skill with a non-zero count, and that count. Unordered.
    pub fn iter(&self) -> impl Iterator<Item = (&str, u32)> {
        self.counts.iter().map(|(name, n)| (name.as_str(), *n))
    }

    /// Invocations counted across every source.
    pub fn total(&self) -> u32 {
        self.sources.iter().map(|s| s.invocations).sum()
    }

    /// How many session files the counts were derived from.
    pub fn files_read(&self) -> usize {
        self.sources.iter().map(|s| s.files).sum()
    }

    /// The plugins this skill was invoked under, if it was ever namespaced.
    ///
    /// Claude Code records a plugin skill as `plugin-name:skill-name`. The count
    /// is filed under the bare name so it matches a directory, and this keeps
    /// the namespace that was thrown away.
    pub fn plugins(&self, skill: &str) -> &[String] {
        self.plugins.get(skill).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// Accounts for one session file, reading as little of it as it can.
///
/// * unchanged (same length and mtime) — reuse the cached counts, read nothing;
/// * grew, mtime not moved backwards — read only the tail;
/// * shrank, mtime moved backwards, or never cached — read the whole file.
///
/// The tail case is safe because both sources append within a session and never
/// rewrite. Length and mtime together are the usual approximation: a file
/// rewritten to exactly its old length within the same millisecond would be
/// missed, which no session file does.
fn load_file(
    source: UsageSource,
    path: &Path,
    cached: Option<&FileRecord>,
    warnings: &mut Vec<String>,
) -> Option<FileRecord> {
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        // A session that ended between listing and reading. Not a problem.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warnings.push(format!("cannot stat {}: {e}", path.display()));
            return None;
        }
    };
    if !meta.is_file() {
        return None;
    }
    let len = meta.len();
    let mtime = mtime_millis(&meta);

    if let Some(previous) = cached {
        if previous.len == len && previous.mtime_ms == mtime {
            return Some(previous.clone());
        }
        if len > previous.len && mtime >= previous.mtime_ms && previous.line_end <= previous.len {
            let tail = scan_file(source, path, previous.line_end, warnings)?;
            let mut counts = previous.counts.clone();
            for (name, n) in tail.counts {
                *counts.entry(name).or_default() += n;
            }
            return Some(FileRecord {
                len,
                mtime_ms: mtime,
                line_end: tail.line_end,
                counts,
            });
        }
    }

    let scan = scan_file(source, path, 0, warnings)?;
    Some(FileRecord {
        len,
        mtime_ms: mtime,
        line_end: scan.line_end,
        counts: scan.counts,
    })
}

/// What reading part or all of one file produced.
struct Scan {
    /// Skill names as recorded, with their counts.
    counts: BTreeMap<String, u32>,
    /// Byte offset just past the last *complete* line. The next load resumes
    /// exactly here, so a line that was still being written is counted once,
    /// when it is whole, and never half-parsed.
    line_end: u64,
}

/// Reads a session file from `from` to the end, counting invocations.
///
/// Returns `None` only when the file could not be opened; a read error partway
/// through keeps whatever was counted so far.
fn scan_file(
    source: UsageSource,
    path: &Path,
    from: u64,
    warnings: &mut Vec<String>,
) -> Option<Scan> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) => {
            warnings.push(format!("cannot read {}: {e}", path.display()));
            return None;
        }
    };
    if from > 0
        && let Err(e) = file.seek(SeekFrom::Start(from))
    {
        warnings.push(format!("cannot read {}: {e}", path.display()));
        return None;
    }

    let mut reader = BufReader::with_capacity(READ_BUFFER, file);
    let mut counts = BTreeMap::new();
    // Resumed sessions re-copy a handful of rows verbatim. Deduplicating by the
    // row's uuid is per file, which is where the duplication happens; an
    // incremental read starts with an empty set, and cannot re-see a row from
    // the part of the file it is deliberately not reading.
    let mut seen = HashSet::new();
    let mut buf = Vec::new();
    let mut position = from;
    let mut line_end = from;

    loop {
        buf.clear();
        let read = match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(read) => read,
            Err(e) => {
                warnings.push(format!("cannot read {}: {e}", path.display()));
                break;
            }
        };
        position += read as u64;
        if buf.last() != Some(&b'\n') {
            // A final line still being written. Leave it for the next load,
            // which resumes at `line_end` and sees it whole.
            break;
        }
        line_end = position;

        if !source.may_mention_a_skill(&buf) {
            continue;
        }
        let Ok(line) = std::str::from_utf8(&buf) else {
            continue;
        };
        source.count_line(line, &mut seen, &mut counts);
    }

    Some(Scan { counts, line_end })
}

/// One row of a Claude Code transcript, reduced to the fields that matter.
///
/// `message.content` stays a [`serde_json::Value`] because its shape is the
/// discriminator: a string means a slash command, an array means content
/// blocks.
#[derive(Deserialize)]
struct TranscriptRow {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    message: Option<TranscriptMessage>,
}

/// The `message` object of a transcript row.
#[derive(Deserialize)]
struct TranscriptMessage {
    #[serde(default)]
    content: Option<serde_json::Value>,
}

/// Counts the skills one transcript line invokes.
///
/// Sidechain rows (`isSidechain: true`, a subagent's own transcript) are
/// counted, deliberately. A skill a subagent loaded ran on this machine, did
/// work and cost tokens; excluding it would understate use by however much of
/// the work is delegated, which on some setups is most of it. The field is not
/// read at all, which is the whole of the decision.
fn count_transcript_line(
    line: &str,
    seen: &mut HashSet<String>,
    counts: &mut BTreeMap<String, u32>,
) {
    let Ok(row) = serde_json::from_str::<TranscriptRow>(line) else {
        return;
    };
    let Some(content) = row.message.and_then(|m| m.content) else {
        return;
    };

    let mut invoked: Vec<&str> = Vec::new();
    if let Some(text) = content.as_str() {
        if row.kind == "user"
            && let Some(name) = slash_command_skill(text)
        {
            invoked.push(name);
        }
    } else if row.kind == "assistant"
        && let Some(blocks) = content.as_array()
    {
        for block in blocks {
            if block.get("type").and_then(serde_json::Value::as_str) == Some("tool_use")
                && block.get("name").and_then(serde_json::Value::as_str) == Some("Skill")
                && let Some(skill) = block
                    .get("input")
                    .and_then(|input| input.get("skill"))
                    .and_then(serde_json::Value::as_str)
                && !skill.is_empty()
            {
                invoked.push(skill);
            }
        }
    }

    if invoked.is_empty() {
        return;
    }
    if let Some(uuid) = row.uuid
        && !seen.insert(uuid)
    {
        return;
    }
    for name in invoked {
        *counts.entry(name.to_string()).or_default() += 1;
    }
}

/// The skill a slash-command row invoked, if it invoked one.
///
/// The row's content is a plain string of the form
///
/// ```text
/// <command-message>name</command-message>
/// <command-name>/the-skill</command-name>
/// <command-args>…</command-args>
/// ```
///
/// The leading `<command-message>` wrapper is the discriminator. Claude Code's
/// own commands — `/model`, `/clear`, `/compact`, `/login` — emit the same tags
/// in the other order, with `<command-name>` first, so requiring the string to
/// *start* with `<command-message>` separates a skill from a built-in.
///
/// It must be a prefix test and never a substring search for `<command-name>`.
/// Transcripts quote each other's text constantly — an assistant explaining what
/// a row looked like reproduces these tags verbatim — and a naive grep
/// over-counts by about two and a half times. Those quotes live in content
/// *arrays*, which this function never sees, and would not match a prefix test
/// even if it did.
///
/// What comes back is a *slash command*, which is not quite the same thing as a
/// skill: a plugin command shares the syntax, and on a real machine a handful of
/// `/doctor` rows arrive in the skill-shaped order. The transcript draws no
/// distinction, so nor can this. It does not matter in practice, because a name
/// is only ever looked up through [`Usage::count`] for a skill
/// [`crate::discovery`] actually found on disk; a command that is not a skill is
/// counted into a bucket nobody reads.
fn slash_command_skill(content: &str) -> Option<&str> {
    let rest = content.strip_prefix("<command-message>")?;
    let open = rest.find("<command-name>")? + "<command-name>".len();
    let value = &rest[open..];
    let name = value[..value.find("</command-name>")?]
        .trim()
        .strip_prefix('/')?;
    (!name.is_empty()).then_some(name)
}

/// One line of a Copilot CLI event log, reduced to what matters.
#[derive(Deserialize)]
struct CopilotEvent {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    data: Option<CopilotEventData>,
}

/// The `data` object of a Copilot event.
#[derive(Deserialize)]
struct CopilotEventData {
    #[serde(default)]
    name: Option<String>,
}

/// Counts the skill one Copilot event invoked.
///
/// There is one signal and no ambiguity: `skill.invoked` carries the name.
/// Copilot writes no row uuid, so there is nothing to deduplicate by — and
/// nothing that duplicates.
fn count_copilot_line(line: &str, counts: &mut BTreeMap<String, u32>) {
    let Ok(event) = serde_json::from_str::<CopilotEvent>(line) else {
        return;
    };
    if event.kind != "skill.invoked" {
        return;
    }
    let Some(name) = event
        .data
        .and_then(|data| data.name)
        .filter(|n| !n.is_empty())
    else {
        return;
    };
    *counts.entry(name).or_default() += 1;
}

/// Splits `plugin-name:skill-name` into its parts.
///
/// The skill name is the part after the last colon, because that is what
/// matches a directory on disk and therefore a name from
/// [`crate::discovery`]. A name without a colon is its own skill name.
fn split_namespace(recorded: &str) -> (Option<&str>, &str) {
    match recorded.rsplit_once(':') {
        Some((plugin, name)) if !name.is_empty() && !plugin.is_empty() => (Some(plugin), name),
        _ => (None, recorded),
    }
}

/// The entries of one directory, sorted, or nothing.
///
/// A directory that does not exist means the agent is not installed, which is
/// the common case and not a warning. Anything else is.
fn list_dir(dir: &Path, warnings: &mut Vec<String>) -> Vec<PathBuf> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            warnings.push(format!("cannot read {}: {e}", dir.display()));
            return Vec::new();
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => paths.push(entry.path()),
            Err(e) => warnings.push(format!("cannot read an entry in {}: {e}", dir.display())),
        }
    }
    paths.sort();
    paths
}

/// Modification time in milliseconds since the epoch, negative before it.
///
/// A file whose mtime cannot be read scores 0, which never compares equal to a
/// cached record and so forces a full re-read: the safe direction.
fn mtime_millis(meta: &fs::Metadata) -> i64 {
    let Ok(modified) = meta.modified() else {
        return 0;
    };
    match modified.duration_since(UNIX_EPOCH) {
        Ok(since) => since.as_millis().min(i64::MAX as u128) as i64,
        Err(before) => -(before.duration().as_millis().min(i64::MAX as u128) as i64),
    }
}

/// True when `haystack` holds `needle`, searching for `needle[anchor]` first.
///
/// `[u8]` has no substring search in the standard library, and
/// `windows(n).any(…)` compares far too much over hundreds of megabytes. This
/// scans for one byte and compares only where it lands, which stays a single
/// pass as long as that byte is rare.
///
/// Which is why the anchor is a parameter rather than always the first byte.
/// The caller picks a byte that is rare *in the haystack*: over JSON a `"` is
/// worthless as a discriminator and appears dozens of times per line, while the
/// `S` of `"Skill"` costs almost nothing.
///
/// `anchor` must be a valid index into `needle`. An empty needle always matches.
fn has_bytes(haystack: &[u8], needle: &[u8], anchor: usize) -> bool {
    if needle.is_empty() {
        return true;
    }
    debug_assert!(anchor < needle.len());
    let wanted = needle[anchor];
    let mut at = anchor;
    while at < haystack.len() {
        let Some(offset) = haystack[at..].iter().position(|b| *b == wanted) else {
            return false;
        };
        let start = at + offset - anchor;
        if start + needle.len() <= haystack.len()
            && haystack[start..start + needle.len()] == *needle
        {
            return true;
        }
        at += offset + 1;
    }
    false
}

/// The counts derived from one session file, and what the file looked like when
/// they were derived.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileRecord {
    /// The file's length when it was last read.
    len: u64,
    /// Its modification time in milliseconds since the epoch.
    mtime_ms: i64,
    /// Byte offset just past the last complete line read. Equal to `len` for a
    /// file ending in a newline, smaller when the last line was still being
    /// written. The resume point for an incremental read.
    line_end: u64,
    /// Skill names as recorded, plugin namespace included, with their counts.
    counts: BTreeMap<String, u32>,
}

/// The on-disk cache: what every session file contributed, last time.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    /// [`CACHE_VERSION`]. A mismatch discards the whole cache.
    version: u32,
    /// Keyed by absolute path.
    #[serde(default)]
    files: BTreeMap<String, FileRecord>,
}

impl Cache {
    /// Reads the cache, or returns an empty one.
    ///
    /// Every failure — missing, unreadable, corrupt, written by another version
    /// — means the same thing: do a full scan. None of them is worth telling
    /// the user about, because none of them changes the answer.
    fn read(path: &Path) -> Cache {
        let Ok(text) = fs::read_to_string(path) else {
            return Cache::default();
        };
        match serde_json::from_str::<Cache>(&text) {
            Ok(cache) if cache.version == CACHE_VERSION => cache,
            _ => Cache::default(),
        }
    }

    /// Writes the cache, and returns a warning line when it could not be
    /// written.
    ///
    /// Written to a sibling and renamed, so a load interrupted midway leaves the
    /// previous cache intact rather than a truncated one.
    ///
    /// Failing here costs no correctness and all of the speed: the next load
    /// reads every transcript on the machine from byte zero again, and the one
    /// after that too. That is a few hundred megabytes on a well used machine,
    /// so it is worth a line in [`Usage::warnings`] rather than a silent
    /// return.
    fn write(path: &Path, files: BTreeMap<String, FileRecord>) -> Result<(), String> {
        let cache = Cache {
            version: CACHE_VERSION,
            files,
        };
        let failed = |e: &dyn std::fmt::Display| {
            format!(
                "cannot write the usage cache {}: {e}; until it can be written, \
                 every launch reads every transcript again",
                path.display()
            )
        };
        let text = serde_json::to_string(&cache).map_err(|e| failed(&e))?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| failed(&e))?;
        }
        let temp = path.with_extension("json.tmp");
        fs::write(&temp, text).map_err(|e| {
            // A half-written temp file left behind is never read and never
            // cleaned up by anything else, so remove it here too.
            let _ = fs::remove_file(&temp);
            failed(&e)
        })?;
        fs::rename(&temp, path).map_err(|e| {
            let _ = fs::remove_file(&temp);
            failed(&e)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixture::Fixture;

    /// An assistant row invoking a skill as a tool.
    fn tool_use(uuid: &str, skill: &str) -> String {
        format!(
            r#"{{"parentUuid":null,"isSidechain":false,"type":"assistant","uuid":"{uuid}",
"timestamp":"2026-09-01T10:00:00.000Z","message":{{"role":"assistant","content":[
{{"type":"tool_use","id":"toolu_1","name":"Skill","input":{{"skill":"{skill}"}}}}]}}}}"#
        )
        .replace('\n', "")
    }

    /// A user row invoking a skill as a slash command.
    fn slash(uuid: &str, skill: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"{uuid}","timestamp":"2026-09-01T10:00:00.000Z",
"message":{{"role":"user","content":"<command-message>{skill}</command-message>\n
<command-name>/{skill}</command-name>\n<command-args>go</command-args>"}}}}"#
        )
        .replace('\n', "")
    }

    /// Writes one Claude Code transcript and returns its absolute path.
    fn transcript(fx: &Fixture, name: &str, rows: &[String]) -> PathBuf {
        let mut text = rows.join("\n");
        text.push('\n');
        fx.write_file(
            &format!("{CLAUDE_TRANSCRIPT_DIR}/project/{name}.jsonl"),
            &text,
        )
    }

    #[test]
    fn a_skill_tool_use_row_is_counted() {
        let fx = Fixture::empty();
        transcript(
            &fx,
            "one",
            &[tool_use("a", "diagnose"), tool_use("b", "diagnose")],
        );
        let usage = Usage::load(&fx.roots());

        assert_eq!(usage.count("diagnose"), 2);
        assert_eq!(usage.count("never-run"), 0);
        assert!(!usage.is_empty());
        assert!(usage.warnings().is_empty(), "{:?}", usage.warnings());
    }

    #[test]
    fn a_slash_command_row_is_counted_as_its_own_signal() {
        let fx = Fixture::empty();
        // A slash command emits no Skill tool_use at all, so this is the only
        // trace it leaves.
        transcript(&fx, "one", &[slash("a", "babysit-pr")]);
        let usage = Usage::load(&fx.roots());
        assert_eq!(usage.count("babysit-pr"), 1);
    }

    #[test]
    fn a_built_in_command_is_not_a_skill() {
        let fx = Fixture::empty();
        // Claude Code's own commands put <command-name> first, with no
        // <command-message> wrapper in front of it.
        let builtin = |name: &str| {
            format!(
                r#"{{"type":"user","uuid":"u-{name}","message":{{"role":"user","content":"<command-name>/{name}</command-name>\n<command-message>{name}</command-message>\n<command-args></command-args>"}}}}"#
            )
        };
        transcript(
            &fx,
            "one",
            &[
                builtin("model"),
                builtin("clear"),
                builtin("compact"),
                tool_use("z", "real-skill"),
            ],
        );
        let usage = Usage::load(&fx.roots());

        assert_eq!(usage.count("model"), 0);
        assert_eq!(usage.count("clear"), 0);
        assert_eq!(usage.count("compact"), 0);
        assert_eq!(usage.count("real-skill"), 1);
        assert_eq!(usage.total(), 1);
    }

    #[test]
    fn a_quoted_command_name_is_not_an_invocation() {
        let fx = Fixture::empty();
        // Transcripts quote each other's text. A substring search for
        // <command-name> counts these; a prefix test on a string content does
        // not, because this content is an array of blocks.
        let quoting = r#"{"type":"assistant","uuid":"q","message":{"role":"assistant","content":[{"type":"text","text":"The row read <command-message>ghost</command-message>\n<command-name>/ghost</command-name>, which is a slash command."}]}}"#;
        let quoting_user = r#"{"type":"user","uuid":"r","message":{"role":"user","content":"here is what it looked like: <command-message>ghost</command-message>\n<command-name>/ghost</command-name>"}}"#;
        transcript(&fx, "one", &[quoting.to_string(), quoting_user.to_string()]);
        let usage = Usage::load(&fx.roots());

        assert_eq!(usage.count("ghost"), 0);
        assert_eq!(usage.total(), 0);
        assert!(
            !usage.is_empty(),
            "a transcript was read; it counted nothing"
        );
    }

    #[test]
    fn a_plugin_namespaced_name_resolves_to_the_skill() {
        let fx = Fixture::empty();
        transcript(
            &fx,
            "one",
            &[
                tool_use("a", "bb-global-skills:bb-plugin-authoring"),
                tool_use("b", "bb-plugin-authoring"),
            ],
        );
        let usage = Usage::load(&fx.roots());

        assert_eq!(
            usage.count("bb-plugin-authoring"),
            2,
            "the namespaced and bare names are the same skill"
        );
        assert_eq!(usage.count("bb-global-skills:bb-plugin-authoring"), 0);
        assert_eq!(
            usage.plugins("bb-plugin-authoring"),
            ["bb-global-skills"],
            "the namespace is kept even though the count is filed under the bare name"
        );
        assert!(usage.plugins("bb-global-skills").is_empty());
    }

    #[test]
    fn a_repeated_uuid_within_one_file_counts_once() {
        let fx = Fixture::empty();
        let row = tool_use("same-uuid", "diagnose");
        transcript(&fx, "one", &[row.clone(), row.clone(), row]);
        let usage = Usage::load(&fx.roots());
        assert_eq!(
            usage.count("diagnose"),
            1,
            "a resumed session re-copies rows"
        );
    }

    #[test]
    fn the_same_uuid_in_two_files_counts_twice() {
        let fx = Fixture::empty();
        transcript(&fx, "one", &[tool_use("shared", "diagnose")]);
        transcript(&fx, "two", &[tool_use("shared", "diagnose")]);
        let usage = Usage::load(&fx.roots());
        assert_eq!(
            usage.count("diagnose"),
            2,
            "deduplication is per file, because that is where duplication happens"
        );
    }

    #[test]
    fn a_sidechain_row_is_counted() {
        let fx = Fixture::empty();
        let sidechain =
            tool_use("s", "diagnose").replace(r#""isSidechain":false"#, r#""isSidechain":true"#);
        assert!(sidechain.contains("true"), "the fixture rewrote the flag");
        transcript(&fx, "one", &[sidechain]);
        let usage = Usage::load(&fx.roots());
        assert_eq!(usage.count("diagnose"), 1, "a subagent's use is a use");
    }

    #[test]
    fn a_copilot_skill_invoked_event_is_counted() {
        let fx = Fixture::empty();
        fx.write_file(
            &format!("{COPILOT_SESSION_DIR}/abc/events.jsonl"),
            "{\"type\":\"session.start\"}\n\
             {\"type\":\"skill.invoked\",\"data\":{\"name\":\"find-skills\",\"path\":\"/h/.agents/skills/find-skills/SKILL.md\"}}\n\
             {\"type\":\"skill.invoked\",\"data\":{\"name\":\"find-skills\"}}\n",
        );
        let usage = Usage::load(&fx.roots());

        assert_eq!(usage.count("find-skills"), 2);
        assert!(usage.has_source(UsageSource::Copilot));
        assert!(!usage.has_source(UsageSource::ClaudeCode));
        assert_eq!(usage.sources().len(), 1);
        assert_eq!(usage.sources()[0].files, 1);
        assert_eq!(usage.sources()[0].invocations, 2);
    }

    #[test]
    fn both_sources_are_reported_separately() {
        let fx = Fixture::empty();
        transcript(&fx, "one", &[tool_use("a", "diagnose")]);
        fx.write_file(
            &format!("{COPILOT_SESSION_DIR}/abc/events.jsonl"),
            "{\"type\":\"skill.invoked\",\"data\":{\"name\":\"diagnose\"}}\n",
        );
        let usage = Usage::load(&fx.roots());

        assert_eq!(usage.count("diagnose"), 2);
        let reported: Vec<_> = usage.sources().iter().map(|s| s.source).collect();
        assert_eq!(
            reported,
            [UsageSource::ClaudeCode, UsageSource::Copilot],
            "in registry order"
        );
        assert_eq!(usage.files_read(), 2);
    }

    #[test]
    fn an_empty_home_has_no_usage_data_at_all() {
        let fx = Fixture::empty();
        let usage = Usage::load(&fx.roots());

        assert!(
            usage.is_empty(),
            "no source could be read, which is not the same as every count being zero"
        );
        assert_eq!(usage.count("anything"), 0);
        assert!(usage.sources().is_empty());
        assert!(
            usage.warnings().is_empty(),
            "an agent that is not installed is not a problem"
        );
    }

    #[test]
    fn a_second_load_reads_only_the_bytes_the_file_grew_by() {
        let fx = Fixture::empty();
        let path = transcript(&fx, "one", &[tool_use("a", "diagnose")]);
        assert_eq!(Usage::load(&fx.roots()).count("diagnose"), 1);

        // Rewrite the already-counted prefix in place, keeping its byte length,
        // and append one row. An incremental read never revisits the prefix, so
        // the rewrite must be invisible; a full re-scan would see it.
        let rewritten = fs::read_to_string(&path)
            .unwrap()
            .replace("\"skill\":\"diagnose\"", "\"skill\":\"REWRITTEN\"");
        let mut text = rewritten;
        text.push_str(&tool_use("b", "diagnose"));
        text.push('\n');
        fs::write(&path, &text).unwrap();

        let usage = Usage::load(&fx.roots());
        assert_eq!(
            usage.count("diagnose"),
            2,
            "the cached prefix count plus exactly one new row"
        );
        assert_eq!(
            usage.count("REWRITTEN"),
            0,
            "the prefix was reused from the cache, not read again"
        );
    }

    #[test]
    fn an_unchanged_file_is_not_read_again() {
        let fx = Fixture::empty();
        let path = transcript(&fx, "one", &[tool_use("a", "diagnose")]);
        assert_eq!(Usage::load(&fx.roots()).count("diagnose"), 1);

        // Same length, same mtime, different bytes: the cache is trusted, which
        // is the approximation every mtime cache makes.
        let before = fs::metadata(&path).unwrap();
        let times = fs::FileTimes::new()
            .set_accessed(
                before
                    .accessed()
                    .unwrap_or_else(|_| before.modified().unwrap()),
            )
            .set_modified(before.modified().unwrap());
        let text = fs::read_to_string(&path)
            .unwrap()
            .replace("\"skill\":\"diagnose\"", "\"skill\":\"REWRITTEN\"");
        fs::write(&path, &text).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(times)
            .unwrap();

        let usage = Usage::load(&fx.roots());
        assert_eq!(usage.count("diagnose"), 1);
        assert_eq!(usage.count("REWRITTEN"), 0);
    }

    #[test]
    fn a_file_that_shrank_is_read_from_the_start() {
        let fx = Fixture::empty();
        let path = transcript(
            &fx,
            "one",
            &[tool_use("a", "diagnose"), tool_use("b", "diagnose")],
        );
        assert_eq!(Usage::load(&fx.roots()).count("diagnose"), 2);

        transcript(&fx, "one", &[tool_use("c", "other")]);
        assert!(fs::metadata(&path).unwrap().len() > 0);

        let usage = Usage::load(&fx.roots());
        assert_eq!(
            usage.count("diagnose"),
            0,
            "the old counts are gone with the bytes"
        );
        assert_eq!(usage.count("other"), 1);
    }

    #[test]
    fn a_pruned_transcript_takes_its_counts_with_it() {
        let fx = Fixture::empty();
        let path = transcript(&fx, "one", &[tool_use("a", "diagnose")]);
        transcript(&fx, "two", &[tool_use("b", "diagnose")]);
        assert_eq!(Usage::load(&fx.roots()).count("diagnose"), 2);

        // Claude Code prunes at ~30 days. Counts decay; that is correct.
        fs::remove_file(&path).unwrap();
        assert_eq!(Usage::load(&fx.roots()).count("diagnose"), 1);
    }

    #[test]
    fn a_line_still_being_written_is_counted_once_it_is_whole() {
        let fx = Fixture::empty();
        let row = tool_use("a", "diagnose");
        let half = &row[..row.len() / 2];
        let path = fx.write_file(&format!("{CLAUDE_TRANSCRIPT_DIR}/project/one.jsonl"), half);
        assert_eq!(
            Usage::load(&fx.roots()).count("diagnose"),
            0,
            "a truncated row is not an invocation"
        );

        fs::write(&path, format!("{row}\n")).unwrap();
        assert_eq!(
            Usage::load(&fx.roots()).count("diagnose"),
            1,
            "and it is not lost either: the resume point is the last complete line"
        );
    }

    #[test]
    fn a_corrupt_cache_falls_back_to_a_full_scan() {
        let fx = Fixture::empty();
        transcript(&fx, "one", &[tool_use("a", "diagnose")]);
        fx.write_file(USAGE_CACHE_FILE, "{ this is not json at all ");

        let usage = Usage::load(&fx.roots());
        assert_eq!(usage.count("diagnose"), 1);
        assert!(
            usage.warnings().is_empty(),
            "a bad cache means 'scan everything', not an error the user sees: {:?}",
            usage.warnings()
        );

        // And it was replaced with a good one, which the next load can use.
        let text = fs::read_to_string(fx.home().join(USAGE_CACHE_FILE)).unwrap();
        assert!(text.contains("\"version\":1"), "{text}");
        assert_eq!(Usage::load(&fx.roots()).count("diagnose"), 1);
    }

    #[test]
    fn a_cache_that_cannot_be_written_warns_and_the_counts_still_arrive() {
        let fx = Fixture::empty();
        transcript(&fx, "one", &[tool_use("a", "diagnose")]);
        // `.skillbase` is a file, so the cache directory cannot be created and
        // the cache cannot be written.
        fx.write_file(".skillbase", "not a directory\n");

        let usage = Usage::load(&fx.roots());

        assert_eq!(usage.count("diagnose"), 1, "the scan still answers");
        let warning = usage
            .warnings()
            .iter()
            .find(|w| w.contains("usage cache"))
            .unwrap_or_else(|| panic!("no cache warning in {:?}", usage.warnings()));
        assert!(warning.contains(USAGE_CACHE_FILE), "{warning}");
        assert!(warning.contains("every launch"), "{warning}");
    }

    #[test]
    fn a_cache_written_without_trouble_says_nothing() {
        let fx = Fixture::empty();
        transcript(&fx, "one", &[tool_use("a", "diagnose")]);

        let usage = Usage::load(&fx.roots());

        assert!(
            usage.warnings().is_empty(),
            "a cache that wrote is not news: {:?}",
            usage.warnings()
        );
        assert!(fx.home().join(USAGE_CACHE_FILE).is_file());
    }

    #[test]
    fn a_cache_from_another_version_is_discarded() {
        let fx = Fixture::empty();
        transcript(&fx, "one", &[tool_use("a", "diagnose")]);
        fx.write_file(USAGE_CACHE_FILE, r#"{"version":99,"files":{}}"#);
        assert_eq!(Usage::load(&fx.roots()).count("diagnose"), 1);
    }

    #[test]
    fn an_unreadable_directory_warns_instead_of_failing() {
        use std::os::unix::fs::PermissionsExt;

        let fx = Fixture::empty();
        transcript(&fx, "one", &[tool_use("a", "diagnose")]);
        fx.write_file(
            &format!("{COPILOT_SESSION_DIR}/abc/events.jsonl"),
            "{\"type\":\"skill.invoked\",\"data\":{\"name\":\"find-skills\"}}\n",
        );
        let locked = fx.home().join(CLAUDE_TRANSCRIPT_DIR).join("project");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let usage = Usage::load(&fx.roots());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            usage.count("find-skills"),
            1,
            "the rest of the scan carried on"
        );
        assert!(
            usage
                .warnings()
                .iter()
                .any(|w| w.starts_with("cannot read")),
            "warnings were {:?}",
            usage.warnings()
        );
    }

    #[test]
    fn only_two_agents_record_anything() {
        use crate::registry::Registry;

        assert_eq!(RECORDING_AGENT_IDS, ["claude-code", "copilot"]);
        for id in RECORDING_AGENT_IDS {
            assert!(Registry::get(id).is_some(), "{id} must be in the table");
        }
        assert!(
            RECORDING_AGENT_IDS.len() * 4 < Registry::all().len(),
            "the point of this module's doc comment: most agents record nothing"
        );
        for source in UsageSource::ALL {
            assert!(RECORDING_AGENT_IDS.contains(&source.agent_id()));
        }
    }

    #[test]
    fn the_prefilter_matches_what_the_parser_needs() {
        assert!(UsageSource::ClaudeCode.may_mention_a_skill(tool_use("a", "x").as_bytes()));
        assert!(UsageSource::ClaudeCode.may_mention_a_skill(slash("a", "x").as_bytes()));
        assert!(
            !UsageSource::ClaudeCode
                .may_mention_a_skill(br#"{"type":"user","message":{"content":"hello"}}"#)
        );
        assert!(UsageSource::Copilot.may_mention_a_skill(br#"{"type":"skill.invoked"}"#));

        // The anchor picks which byte is searched for; every anchor into the
        // needle must give the same answer.
        for anchor in 0..3 {
            assert!(has_bytes(b"abcdef", b"cde", anchor), "{anchor}");
            assert!(has_bytes(b"aaab", b"aab", anchor), "{anchor}");
            assert!(!has_bytes(b"abc", b"abd", anchor), "{anchor}");
            assert!(!has_bytes(b"ab", b"abc", anchor), "{anchor}");
            assert!(!has_bytes(b"", b"abc", anchor), "{anchor}");
            assert!(has_bytes(b"xxxabcxxx", b"abc", anchor), "{anchor}");
            assert!(has_bytes(b"abc", b"abc", anchor), "{anchor}");
        }
        assert!(has_bytes(b"abc", b"", 0));
        assert!(has_bytes(b"\"name\":\"Skill\"", b"\"Skill\"", 1));
    }

    #[test]
    fn namespaces_split_at_the_last_colon() {
        assert_eq!(split_namespace("a:b"), (Some("a"), "b"));
        assert_eq!(split_namespace("a:b:c"), (Some("a:b"), "c"));
        assert_eq!(split_namespace("plain"), (None, "plain"));
        assert_eq!(split_namespace(":b"), (None, ":b"));
        assert_eq!(split_namespace("a:"), (None, "a:"));
    }

    #[test]
    fn slash_commands_are_recognized_by_their_leading_wrapper() {
        assert_eq!(
            slash_command_skill(
                "<command-message>x</command-message>\n<command-name>/my-skill</command-name>"
            ),
            Some("my-skill")
        );
        assert_eq!(
            slash_command_skill(
                "<command-name>/model</command-name>\n<command-message>model</command-message>"
            ),
            None,
            "a built-in puts the name first"
        );
        assert_eq!(
            slash_command_skill("look: <command-name>/quoted</command-name>"),
            None
        );
        assert_eq!(
            slash_command_skill(
                "<command-message>x</command-message>\n<command-name>no-slash</command-name>"
            ),
            None
        );
        assert_eq!(
            slash_command_skill(
                "<command-message>x</command-message>\n<command-name>/</command-name>"
            ),
            None
        );
    }

    /// Counts usage on the real machine and reports how long it took.
    ///
    /// Ignored by default, because it depends on the machine it runs on. Run it
    /// with
    /// `cargo test -p skillbase-core -- --ignored --nocapture real_machine_usage`.
    /// It reads the transcripts and writes only `~/.skillbase/usage.json`.
    #[test]
    #[ignore = "depends on the machine it runs on"]
    fn real_machine_usage() {
        let roots = Roots::discover().expect("a home directory");
        let cache = roots.home().join(USAGE_CACHE_FILE);
        let _ = fs::remove_file(&cache);

        let cold = std::time::Instant::now();
        let usage = Usage::load(&roots);
        let cold = cold.elapsed();

        let warm = std::time::Instant::now();
        let again = Usage::load(&roots);
        let warm = warm.elapsed();

        println!("home: {}", roots.home().display());
        println!("full scan   {cold:?}");
        println!("cached load {warm:?}");
        println!("\nsources:");
        for stat in usage.sources() {
            println!(
                "  {:<16} {:>4} files  {:>5} invocations",
                stat.source.display_name(),
                stat.files,
                stat.invocations
            );
        }
        assert_eq!(usage.total(), again.total(), "a cached load must agree");

        let mut ranked: Vec<_> = usage.iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        println!(
            "\n{} skills used, {} invocations:",
            ranked.len(),
            usage.total()
        );
        for (name, count) in ranked.iter().take(25) {
            let plugins = usage.plugins(name);
            let from = if plugins.is_empty() {
                String::new()
            } else {
                format!("   (plugin {})", plugins.join(", "))
            };
            println!("  {count:>5}  {name}{from}");
        }

        println!("\n{} warnings:", usage.warnings().len());
        for warning in usage.warnings() {
            println!("  {warning}");
        }
    }
}
