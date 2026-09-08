//! The view model: one read-only scan of the filesystem, mapped into the shape
//! the three panes render.
//!
//! Everything here is plain data and pure functions. The scan itself is the one
//! function that touches the filesystem, and it is called from a background
//! task, never from `render`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use gpui_kit::SharedString;
use skillbase_core::{
    AgentDef, CLAUDE_RETENTION_DAYS, DisableMode, DiscoveredSkill, Discovery, Location,
    LocationKind, PRIVATE_ID, Provenance, RECORDING_AGENT_IDS, Registry, Roots, SHARED_ID,
    SkillError, SkillLock, UpdateTarget, provenance_of,
};

/// Environment variable that points Skillbase at a different home directory.
///
/// Every path the application reads or writes is resolved from one [`Roots`],
/// so setting this redirects discovery *and* every mutation together. It exists
/// so that link, adopt, release and delete can be exercised against a throwaway
/// tree instead of the user's real skills.
pub const HOME_OVERRIDE_ENV: &str = "SKILLBASE_HOME";

/// The home directory Skillbase runs against, and whether it was overridden.
///
/// A `true` second element means the application is not looking at the real
/// home, which the title bar says out loud.
pub fn resolve_roots() -> Result<(Roots, bool), SkillError> {
    match std::env::var_os(HOME_OVERRIDE_ENV) {
        Some(home) if !home.is_empty() => Ok((Roots::new(PathBuf::from(home)), true)),
        _ => Ok((Roots::discover()?, false)),
    }
}

/// The file the "Show all agents" preference is kept in.
///
/// It sits beside the store rather than in the platform's configuration
/// directory, so that it travels with [`HOME_OVERRIDE_ENV`]: a run against a
/// throwaway home gets its own preferences and cannot rewrite the real ones.
pub const SETTINGS_FILE: &str = ".skillbase/settings.json";

/// How the skill list is ordered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SkillSort {
    /// Alphabetical, which is the order a directory listing gives and the one
    /// a reader can predict.
    #[default]
    Name,
    /// Most-invoked first, from the session records agents leave behind.
    MostUsed,
}

impl SkillSort {
    /// Every ordering, in the order the menu lists them.
    pub const ALL: [SkillSort; 2] = [SkillSort::Name, SkillSort::MostUsed];

    /// What the menu calls it.
    pub fn label(self) -> &'static str {
        match self {
            SkillSort::Name => "Name",
            SkillSort::MostUsed => "Most used",
        }
    }

    /// Where the ordering gets its numbers, when that is not obvious.
    ///
    /// A count of zero is the usual case and reads as "never used", which is
    /// not what it means: only two of the agents record invocations at all,
    /// and the one that records the most prunes its transcripts. The sentence
    /// belongs beside the choice that produces the number, not only in
    /// Settings.
    pub fn description(self) -> Option<SharedString> {
        match self {
            SkillSort::Name => None,
            SkillSort::MostUsed => {
                let sources: Vec<&'static str> = RECORDING_AGENT_IDS
                    .iter()
                    .copied()
                    .map(agent_label)
                    .collect();
                Some(
                    format!(
                        "Counted from {} session records. Claude Code prunes its own after \
                         {CLAUDE_RETENTION_DAYS} days, so this is recent history.",
                        join_and(&sources)
                    )
                    .into(),
                )
            }
        }
    }

    /// How it is spelled in the settings file.
    fn key(self) -> &'static str {
        match self {
            SkillSort::Name => "name",
            SkillSort::MostUsed => "most-used",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|sort| sort.key() == key)
    }
}

/// Where a window sat, relative to the display it was on: `x` from that
/// display's left edge and `y` from its top edge.
///
/// Not a desktop-wide coordinate. macOS reports a window's position relative to
/// its own screen and places a new window by the same measure, so a saved frame
/// means nothing without the display it was measured against — which is why
/// [`Preferences::display`] is saved beside it.
///
/// Plain `f32` rather than `Bounds<Pixels>` so that the fitting below is a
/// pure function this module can test without a window or a display.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowFrame {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl WindowFrame {
    /// True when every number is finite and the frame has area.
    ///
    /// The settings file is meant to be readable, which means it is also
    /// editable, and `"window_width": 0`, or a `"window_x": 1e400` that reads
    /// back as an infinity, would otherwise open a window with nothing in it.
    fn is_sane(self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|n| n.is_finite())
            && self.width > 0.
            && self.height > 0.
    }
}

/// Fits a remembered window onto the display it is about to open on.
///
/// A saved frame only describes the desktop that existed the last time
/// Skillbase ran: the display may have been opened at a smaller resolution, the
/// window may be opening on a different display because the one it was on is
/// unplugged, and the file may have been edited by hand. `display` is the area
/// of that display a window may use, measured the same way `frame` is — from
/// the display's own top-left corner — so what comes back is always a window
/// the user can reach and drag.
///
/// `None` means "no usable answer" — a frame that is not a frame, or a display
/// with no area — and the caller should fall back to a centred default rather
/// than guess.
pub fn fit_to_display(
    frame: WindowFrame,
    display: WindowFrame,
    min: (f32, f32),
) -> Option<WindowFrame> {
    if !frame.is_sane() || !display.is_sane() {
        return None;
    }

    // A minimum that does not fit the display is not a minimum. Shrinking to
    // the screen is the lesser evil: a window wider than the screen has its
    // right-hand pane off the edge with no way to drag it back.
    let width = frame.width.min(display.width).max(min.0.min(display.width));
    let height = frame
        .height
        .min(display.height)
        .max(min.1.min(display.height));

    Some(WindowFrame {
        x: frame.x.clamp(display.x, display.x + display.width - width),
        y: frame
            .y
            .clamp(display.y, display.y + display.height - height),
        width,
        height,
    })
}

/// The preferences Skillbase keeps between runs.
///
/// Everything a user would arrange once and expect to find again: how the list
/// is sorted and filtered, what was open, how wide the panes are and where the
/// window was. Written as JSON so the file is readable and can grow keys later
/// without a format change.
#[derive(Clone, Debug, PartialEq)]
pub struct Preferences {
    /// Show every agent in the sidebar, including those with no directory on
    /// this machine. Off by default, per SPEC §5.1.
    pub show_all_agents: bool,
    /// How the skill list is ordered.
    pub sort: SkillSort,
    /// Which sidebar row the list was showing.
    pub scope: Scope,
    /// The skill the detail pane had open, by name.
    ///
    /// A name rather than a path, because a skill that was adopted or moved
    /// between launches is still the same skill. It may also be gone
    /// altogether, which is why restoring it goes through the same fallback a
    /// re-scan uses.
    pub selected: Option<String>,
    /// Whether the sidebar was hidden.
    pub sidebar_collapsed: bool,
    /// Whether the Agents group in the sidebar was closed. Closed until the
    /// user opens it: the Library rows are the usual destination.
    pub agents_collapsed: bool,
    /// How wide the skill list was, in pixels, when the user had dragged the
    /// split away from its default.
    pub list_width: Option<f32>,
    /// Where the window was, relative to the display named below. `None` before
    /// the first run has placed one.
    pub window: Option<WindowFrame>,
    /// Which display the window was on, by the UUID the platform gives it.
    ///
    /// The frame above is measured from a display's own corner, so it lands
    /// somewhere entirely different on a display of another size or in another
    /// position. A UUID rather than the display id: the id is handed out by the
    /// window server and is a different number after a reboot or after a
    /// monitor has been unplugged, while the UUID belongs to the display
    /// itself.
    pub display: Option<String>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            show_all_agents: false,
            sort: SkillSort::default(),
            scope: Scope::Library(Library::All),
            selected: None,
            sidebar_collapsed: false,
            agents_collapsed: true,
            list_width: None,
            window: None,
            display: None,
        }
    }
}

/// What a read of the settings file produced.
pub struct LoadedPreferences {
    pub preferences: Preferences,
    /// Why the file could not be read, when it was there and could not be.
    ///
    /// A file that is absent says "no preference expressed" and is not a
    /// problem. One that is present and unreadable is: Skillbase starts on the
    /// defaults either way, and the next change of any setting writes the file
    /// back, so whatever the user had put in it is about to be lost. That is
    /// worth the same sentence the write failure gets.
    pub problem: Option<SharedString>,
}

impl Preferences {
    /// Where the file lives under this home.
    pub fn path(roots: &Roots) -> PathBuf {
        roots.home().join(SETTINGS_FILE)
    }

    /// Reads the preferences, falling back to the defaults for anything the
    /// file does not say.
    ///
    /// Every key is read independently: one setting this build no longer
    /// understands does not cost the user the rest of them.
    pub fn load(roots: &Roots) -> LoadedPreferences {
        let path = Self::path(roots);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            // Nothing has been saved yet. That is the first run, not a fault.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return LoadedPreferences {
                    preferences: Self::default(),
                    problem: None,
                };
            }
            Err(error) => {
                return LoadedPreferences {
                    preferences: Self::default(),
                    problem: Some(format!("{}: {error}", path.display()).into()),
                };
            }
        };

        let settings = match parse_settings(&text) {
            Ok(settings) => settings,
            Err(problem) => {
                return LoadedPreferences {
                    preferences: Self::default(),
                    problem: Some(format!("{}: {problem}", path.display()).into()),
                };
            }
        };
        LoadedPreferences {
            preferences: Self::from_settings(&settings),
            problem: None,
        }
    }

    fn from_settings(settings: &Settings) -> Self {
        let default = Self::default();
        // All four or none: three quarters of a frame is not a place.
        let window = (|| {
            Some(WindowFrame {
                x: settings.f32_at("window_x")?,
                y: settings.f32_at("window_y")?,
                width: settings.f32_at("window_width")?,
                height: settings.f32_at("window_height")?,
            })
        })()
        .filter(|frame| frame.is_sane());

        Self {
            show_all_agents: settings
                .bool_at("show_all_agents")
                .unwrap_or(default.show_all_agents),
            sort: settings
                .str_at("sort")
                .and_then(SkillSort::from_key)
                .unwrap_or(default.sort),
            scope: settings
                .str_at("scope")
                .and_then(Scope::from_key)
                .unwrap_or(default.scope),
            selected: settings.str_at("selected").map(str::to_string),
            sidebar_collapsed: settings
                .bool_at("sidebar_collapsed")
                .unwrap_or(default.sidebar_collapsed),
            agents_collapsed: settings
                .bool_at("agents_collapsed")
                .unwrap_or(default.agents_collapsed),
            list_width: settings
                .f32_at("list_width")
                .filter(|width| width.is_finite() && *width > 0.),
            window,
            display: settings.str_at("window_display").map(str::to_string),
        }
    }

    /// The JSON this writes and [`Preferences::load`] reads back.
    fn to_json(&self) -> String {
        let mut out = String::from("{\n");
        let mut fields: Vec<String> = vec![
            format!("  \"show_all_agents\": {}", self.show_all_agents),
            format!("  \"sort\": {}", quote(self.sort.key())),
            format!("  \"scope\": {}", quote(&self.scope.key())),
            format!("  \"sidebar_collapsed\": {}", self.sidebar_collapsed),
            format!("  \"agents_collapsed\": {}", self.agents_collapsed),
        ];
        if let Some(selected) = &self.selected {
            fields.push(format!("  \"selected\": {}", quote(selected)));
        }
        if let Some(width) = self.list_width.filter(|w| w.is_finite()) {
            fields.push(format!("  \"list_width\": {width}"));
        }
        // Flat keys rather than a nested object, so the file stays one level
        // deep and the reader below stays a reader rather than a parser.
        if let Some(frame) = self.window.filter(|frame| frame.is_sane()) {
            fields.push(format!("  \"window_x\": {}", frame.x));
            fields.push(format!("  \"window_y\": {}", frame.y));
            fields.push(format!("  \"window_width\": {}", frame.width));
            fields.push(format!("  \"window_height\": {}", frame.height));
        }
        if let Some(display) = &self.display {
            fields.push(format!("  \"window_display\": {}", quote(display)));
        }
        out.push_str(&fields.join(",\n"));
        out.push_str("\n}\n");
        out
    }

    /// Writes the preferences, creating `~/.skillbase` if it is missing.
    ///
    /// Written to a sibling and renamed, the way the usage cache is: a plain
    /// write truncates the file first, so a process killed a moment later
    /// leaves an empty or half-written one, which [`parse_settings`] rejects
    /// outright. That costs the user every preference in the file and a warning
    /// on the next launch.
    ///
    /// Blocking. Call it from a background task.
    pub fn save(&self, roots: &Roots) -> std::io::Result<PathBuf> {
        // Two writers reach here: the debounced one that follows a drag, and
        // the direct one a menu change makes. Both run on background threads
        // and would otherwise be writing the same temp file at the same time,
        // which is how a mixture of the two gets renamed into place.
        static WRITING: Mutex<()> = Mutex::new(());
        // A poisoned lock means an earlier writer panicked. The file on disk is
        // whatever it was; refusing to write from here on would be worse.
        let _writing = WRITING.lock().unwrap_or_else(|held| held.into_inner());

        let path = Self::path(roots);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp = path.with_extension("json.tmp");
        if let Err(error) = fs::write(&temp, self.to_json()) {
            // Nothing else reads or removes a leftover temp file.
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
        if let Err(error) = fs::rename(&temp, &path) {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
        Ok(path)
    }
}

/// One value from the settings file.
///
/// Scalars only: a preference is one answer, and neither the writer above nor
/// any reader below has a use for an array or a nested object.
#[derive(Clone, Debug, PartialEq)]
enum Json {
    Bool(bool),
    Number(f64),
    Str(String),
    Null,
}

/// The settings file, read into its top-level keys.
type Settings = HashMap<String, Json>;

trait SettingsExt {
    fn bool_at(&self, key: &str) -> Option<bool>;
    fn str_at(&self, key: &str) -> Option<&str>;
    fn f32_at(&self, key: &str) -> Option<f32>;
}

impl SettingsExt for Settings {
    fn bool_at(&self, key: &str) -> Option<bool> {
        match self.get(key)? {
            Json::Bool(value) => Some(*value),
            _ => None,
        }
    }

    fn str_at(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Json::Str(value) => Some(value),
            _ => None,
        }
    }

    fn f32_at(&self, key: &str) -> Option<f32> {
        match self.get(key)? {
            Json::Number(value) => Some(*value as f32),
            _ => None,
        }
    }
}

/// Reads the settings file into its top-level keys.
///
/// A strict reader of the subset [`Preferences::to_json`] writes: one object,
/// whose values are strings, numbers, booleans or null. It is strict on
/// purpose. The looser reader this replaced searched the text for `"key":` and
/// so could find a key inside a value, and had no way to read a number; and
/// because it treated every failure as "absent", a file the user had edited
/// into invalid JSON reverted silently and was then overwritten. Failing here
/// is what lets [`Preferences::load`] say so.
///
/// The error is a sentence for the user, so it names what was expected rather
/// than an offset into a file they have to count through.
fn parse_settings(text: &str) -> Result<Settings, String> {
    let mut rest = text.trim_start_matches('\u{feff}');
    let mut settings = Settings::new();
    skip_space(&mut rest);
    expect(&mut rest, '{')?;
    skip_space(&mut rest);
    if !take(&mut rest, '}') {
        loop {
            skip_space(&mut rest);
            let key = parse_string(&mut rest)?;
            skip_space(&mut rest);
            expect(&mut rest, ':')?;
            skip_space(&mut rest);
            let value = parse_value(&mut rest)?;
            settings.insert(key, value);
            skip_space(&mut rest);
            if take(&mut rest, ',') {
                continue;
            }
            expect(&mut rest, '}')?;
            break;
        }
    }
    skip_space(&mut rest);
    if !rest.is_empty() {
        return Err("expected nothing after the closing `}`".into());
    }
    Ok(settings)
}

fn parse_value(rest: &mut &str) -> Result<Json, String> {
    match rest.chars().next() {
        Some('"') => parse_string(rest).map(Json::Str),
        Some('t') if rest.starts_with("true") => {
            *rest = &rest[4..];
            Ok(Json::Bool(true))
        }
        Some('f') if rest.starts_with("false") => {
            *rest = &rest[5..];
            Ok(Json::Bool(false))
        }
        Some('n') if rest.starts_with("null") => {
            *rest = &rest[4..];
            Ok(Json::Null)
        }
        Some(first) if first == '-' || first.is_ascii_digit() => parse_number(rest),
        Some(first) => Err(format!("expected a value, found `{first}`")),
        None => Err("expected a value, found the end of the file".into()),
    }
}

fn parse_number(rest: &mut &str) -> Result<Json, String> {
    let end = rest
        .find(|c: char| !matches!(c, '-' | '+' | '.' | 'e' | 'E' | '0'..='9'))
        .unwrap_or(rest.len());
    let (number, tail) = rest.split_at(end);
    *rest = tail;
    number
        .parse::<f64>()
        .map(Json::Number)
        .map_err(|_| format!("expected a number, found `{number}`"))
}

fn parse_string(rest: &mut &str) -> Result<String, String> {
    expect(rest, '"')?;
    let mut out = String::new();
    let mut chars = rest.char_indices();
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => {
                *rest = &rest[at + 1..];
                return Ok(out);
            }
            '\\' => {
                let (_, escape) = chars.next().ok_or("a string was left unterminated")?;
                match escape {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    '/' => out.push('/'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'u' => out.push(parse_unicode_escape(&mut chars)?),
                    other => return Err(format!("`\\{other}` is not an escape")),
                }
            }
            other => out.push(other),
        }
    }
    Err("a string was left unterminated".into())
}

/// The character after a `\u`, joining a surrogate pair when one follows.
fn parse_unicode_escape(chars: &mut std::str::CharIndices<'_>) -> Result<char, String> {
    let first = take_hex4(chars)?;
    if let Some(scalar) = char::from_u32(first) {
        return Ok(scalar);
    }
    // A high surrogate is only half a character; JSON spells the rest of it as
    // a second `\u` escape immediately after.
    const HIGH: std::ops::Range<u32> = 0xD800..0xDC00;
    const LOW: std::ops::Range<u32> = 0xDC00..0xE000;
    if !HIGH.contains(&first) {
        return Err("a `\\u` escape names half a character".into());
    }
    let unpaired = || "a `\\u` escape names half a character".to_string();
    if !(chars.next().map(|(_, c)| c) == Some('\\') && chars.next().map(|(_, c)| c) == Some('u')) {
        return Err(unpaired());
    }
    let second = take_hex4(chars)?;
    if !LOW.contains(&second) {
        return Err(unpaired());
    }
    let scalar = 0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00);
    char::from_u32(scalar).ok_or_else(unpaired)
}

fn take_hex4(chars: &mut std::str::CharIndices<'_>) -> Result<u32, String> {
    let mut digits = String::with_capacity(4);
    for _ in 0..4 {
        digits.push(chars.next().ok_or("a `\\u` escape was cut short")?.1);
    }
    u32::from_str_radix(&digits, 16).map_err(|_| format!("`\\u{digits}` is not hexadecimal"))
}

fn skip_space(rest: &mut &str) {
    *rest = rest.trim_start_matches([' ', '\t', '\n', '\r']);
}

/// Consumes `wanted` if it is next, and says whether it was.
fn take(rest: &mut &str, wanted: char) -> bool {
    match rest.strip_prefix(wanted) {
        Some(tail) => {
            *rest = tail;
            true
        }
        None => false,
    }
}

fn expect(rest: &mut &str, wanted: char) -> Result<(), String> {
    if take(rest, wanted) {
        return Ok(());
    }
    match rest.chars().next() {
        Some(found) => Err(format!("expected `{wanted}`, found `{found}`")),
        None => Err(format!("expected `{wanted}`, found the end of the file")),
    }
}

/// One JSON string literal, with the characters that cannot stand for
/// themselves escaped.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One directory Skillbase reads, and whether it is there.
///
/// This is the "where does all this actually live" answer the Settings pane
/// gives. The `exists` flag is read once, on the scan's background thread,
/// because `render` may not touch the filesystem.
#[derive(Clone, Debug)]
pub struct DirStatus {
    /// The scope id: [`PRIVATE_ID`], or an agent id.
    pub id: &'static str,
    /// What to call it on screen.
    pub label: SharedString,
    pub path: PathBuf,
    pub exists: bool,
}

/// The rows of the sidebar's Library group.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Library {
    All,
    Shared,
    Managed,
    Unmanaged,
    /// Skills GitHub has moved on from since they were installed.
    Updates,
    Invalid,
    Conflicts,
}

impl Library {
    pub const ALL: [Library; 7] = [
        Library::All,
        Library::Shared,
        Library::Managed,
        Library::Unmanaged,
        Library::Updates,
        Library::Invalid,
        Library::Conflicts,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Library::All => "All",
            Library::Shared => "Shared",
            Library::Managed => "Managed",
            Library::Unmanaged => "Unmanaged",
            Library::Updates => "Updates",
            Library::Invalid => "Invalid",
            // "Duplicates" says what the group holds. The detail pane calls
            // the same thing a duplicate copy, and one operation should not
            // have two names.
            Library::Conflicts => "Duplicates",
        }
    }

    /// How a skill is recognised as this group. Shown in the list header's
    /// help, so the wording matches what the filter actually tests.
    pub fn explanation(self) -> &'static str {
        match self {
            Library::All => "Every skill Skillbase found on this machine.",
            Library::Shared => {
                "The origin is in ~/.agents/skills, which most agents read on their own. \
                 That directory is Skillbase's store, so a skill here is already managed."
            }
            Library::Managed => {
                "The origin is in the store or in ~/.skillbase/private. Skillbase owns \
                 the directory, so it can add and remove the links other agents follow."
            }
            Library::Unmanaged => {
                "The origin sits somewhere Skillbase does not own. You can read and edit \
                 it in place; visibility stays as you found it until you Adopt it."
            }
            Library::Updates => {
                "The last check found a newer copy on GitHub, or found that nothing \
                 recorded which copy was installed, so Skillbase cannot say this one is \
                 current. The group is empty until a check has run."
            }
            Library::Invalid => {
                "SKILL.md is not a frontmatter document Skillbase can parse. The skill \
                 still appears so you can open the file and fix it."
            }
            Library::Conflicts => {
                "The same name exists as more than one real directory. Copies that \
                 drifted are listed rather than silently merged."
            }
        }
    }

    fn index(self) -> usize {
        Library::ALL.iter().position(|l| *l == self).unwrap_or(0)
    }

    /// How the row is spelled in the settings file.
    ///
    /// A file format, not a label: `Conflicts` is called "Duplicates" on
    /// screen, and renaming what the user reads must not silently move
    /// everyone's saved scope.
    fn key(self) -> &'static str {
        match self {
            Library::All => "all",
            Library::Shared => "shared",
            Library::Managed => "managed",
            Library::Unmanaged => "unmanaged",
            Library::Updates => "updates",
            Library::Invalid => "invalid",
            Library::Conflicts => "conflicts",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Library::ALL.into_iter().find(|row| row.key() == key)
    }

    /// Whether this group lists the skill.
    ///
    /// `has_update` answers the one question the scan cannot: whether GitHub
    /// has moved on. That comes from a check made over the network long after
    /// the filesystem was read, so it is passed in rather than recorded on the
    /// skill.
    fn matches(self, skill: &SkillView, has_update: &dyn Fn(&str) -> bool) -> bool {
        match self {
            Library::All => true,
            Library::Shared => skill.in_shared,
            Library::Managed => skill.managed,
            Library::Unmanaged => !skill.managed,
            Library::Updates => has_update(&skill.name),
            Library::Invalid => skill.parse_error.is_some(),
            Library::Conflicts => !skill.conflicts.is_empty(),
        }
    }
}

/// What the skill list is currently showing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Library(Library),
    /// Every skill the agent with this id can currently see.
    Agent(&'static str),
}

impl Scope {
    /// The heading the title bar shows for this scope.
    pub fn title(self) -> &'static str {
        match self {
            Scope::Library(library) => library.label(),
            Scope::Agent(id) => agent_label(id),
        }
    }

    /// Whether this scope lists the given skill.
    ///
    /// `has_update` is asked for [`Library::Updates`], the one group whose
    /// membership the scan cannot settle. There is no answer-free version of
    /// this: a caller that guessed would decide the selection survives a change
    /// of scope while the list, which goes through [`Scan::filter`] with the
    /// real answer, leaves the row out.
    pub fn shows_with(self, skill: &SkillView, has_update: &dyn Fn(&str) -> bool) -> bool {
        match self {
            Scope::Library(library) => library.matches(skill, has_update),
            Scope::Agent(id) => skill.visible_to.contains(&id),
        }
    }

    /// How the scope is spelled in the settings file.
    ///
    /// Prefixed, because a Library row and an agent are two namespaces that
    /// happen to share a sidebar: nothing stops an agent one day being called
    /// `all`.
    fn key(self) -> String {
        match self {
            Scope::Library(library) => format!("library:{}", library.key()),
            Scope::Agent(id) => format!("agent:{id}"),
        }
    }

    /// The scope a settings file names, when this build still has that row.
    ///
    /// An agent dropped from the registry, or a Library row renamed since the
    /// file was written, reads as "no preference expressed": better to open on
    /// the whole library than on a row the sidebar does not show.
    fn from_key(key: &str) -> Option<Self> {
        if let Some(row) = key.strip_prefix("library:") {
            return Library::from_key(row).map(Scope::Library);
        }
        let id = key.strip_prefix("agent:")?;
        Registry::get(id).map(|agent| Scope::Agent(agent.id))
    }
}

/// An agent's display name, by id.
pub fn agent_label(id: &str) -> &'static str {
    Registry::get(id)
        .map(|agent| agent.display_name)
        .unwrap_or("Unknown")
}

/// One validation finding, ready to render beside the field it concerns.
#[derive(Clone, Debug)]
pub struct Issue {
    /// The frontmatter key it concerns, when it concerns one.
    pub field: Option<&'static str>,
    /// True when the finding makes the skill invalid rather than merely odd.
    pub is_error: bool,
    pub message: SharedString,
}

/// One skill, flattened from a [`DiscoveredSkill`] into what the panes need.
///
/// It carries its locations so that a delete plan can be recomputed from it
/// without holding the whole scan.
#[derive(Clone, Debug)]
pub struct SkillView {
    pub name: SharedString,
    pub description: SharedString,
    /// The real directory holding the bytes.
    pub origin: PathBuf,
    /// True when the origin is inside `~/.skillbase/store`.
    pub managed: bool,
    /// Why `SKILL.md` could not be parsed, when it could not.
    pub parse_error: Option<SharedString>,
    /// Validation problems, each tagged with the field that caused it.
    pub issues: Vec<Issue>,
    /// Other real directories claiming the same name.
    pub conflicts: Vec<PathBuf>,
    /// Every path this skill was found at.
    pub locations: Vec<Location>,
    /// True when `~/.codex/config.toml` switches this skill off.
    pub codex_disabled: bool,
    /// Ids of the agents that can currently reach this skill.
    pub visible_to: Vec<&'static str>,
    /// True when an active link sits in `~/.agents/skills`.
    pub in_shared: bool,
    /// Where the skill came from, when it says so. Read from its own
    /// frontmatter first and from the `npx skills` lockfile second; most
    /// skills were written by hand and carry neither, which is why this is an
    /// option rather than a default.
    pub provenance: Option<Provenance>,
}

impl SkillView {
    /// True when this agent holds a location of its own, active or parked in
    /// its disabled directory.
    ///
    /// This is *presence*, which for Claude Code and Codex is a different
    /// question from whether the skill is switched on.
    pub fn linked_to(&self, agent_id: &str) -> bool {
        self.locations.iter().any(|l| l.agent_id == agent_id)
    }

    /// What this agent's own location is on disk, when it has one.
    pub fn location_kind(&self, agent_id: &str) -> Option<&LocationKind> {
        self.locations
            .iter()
            .find(|l| l.agent_id == agent_id)
            .map(|l| &l.kind)
    }

    /// True when this agent's link is parked in its disabled directory.
    pub fn parked_in(&self, agent_id: &str) -> bool {
        self.locations
            .iter()
            .any(|l| l.agent_id == agent_id && matches!(l.kind, LocationKind::Disabled))
    }

    /// True when this agent reaches the skill only because it reads the shared
    /// directory, without a link of its own.
    pub fn via_shared(&self, agent: &AgentDef) -> bool {
        agent.reads_shared && self.in_shared && !self.linked_to(agent.id)
    }

    /// Whether the skill is switched on for an agent that draws a distinction
    /// between present and active.
    ///
    /// [`DisableMode::RemoveLink`] agents draw no such distinction, so for them
    /// this is the same question as presence and the interface does not offer
    /// a second switch.
    pub fn enabled_for(&self, agent: &AgentDef) -> bool {
        match agent.disable {
            DisableMode::RemoveLink => self.linked_to(agent.id) || self.via_shared(agent),
            DisableMode::MoveAside(_) => !self.parked_in(agent.id),
            DisableMode::CodexConfig => !self.codex_disabled,
        }
    }

    /// The agents this skill reaches, in the order the registry lists them.
    ///
    /// `visible_to` is built by walking the registry, so this is normally the
    /// same order; sorting here keeps a row's icons in one order whether or not
    /// that stays true.
    pub fn reach(&self) -> Vec<&'static AgentDef> {
        let mut agents: Vec<&'static AgentDef> = self
            .visible_to
            .iter()
            .filter_map(|id| Registry::get(id))
            .collect();
        agents.sort_by_key(|agent| Registry::order_of(agent.id));
        agents
    }

    fn matches_query(&self, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        let query = query.to_lowercase();
        self.name.to_lowercase().contains(&query)
            || self.description.to_lowercase().contains(&query)
    }
}

/// True when an agent has a real "present but switched off" state, as opposed
/// to one where disabling just means removing the link.
pub fn has_disable_state(agent: &AgentDef) -> bool {
    !matches!(agent.disable, DisableMode::RemoveLink)
}

/// One scan of the filesystem, mapped for display.
///
/// Built on a background thread by [`Scan::load`] and then shared by reference;
/// nothing in it touches the filesystem again.
#[derive(Debug, Default)]
pub struct Scan {
    pub skills: Vec<SkillView>,
    /// Everything discovery could not read, in the order it was noticed.
    pub warnings: Vec<SharedString>,
    /// Agents whose global directory exists on this machine, in registry
    /// order, without the shared scope.
    pub installed: Vec<&'static AgentDef>,
    /// Every directory the scan looked in, and whether it is there: the store
    /// first, then the shared directory, then one row per agent.
    pub dirs: Vec<DirStatus>,
    /// How many skills each agent can see, by id.
    pub agent_counts: HashMap<&'static str, usize>,
    /// How many skills each Library row lists, in [`Library::ALL`] order.
    pub library_counts: [usize; Library::ALL.len()],
    /// Every skill that names where it came from, ready to be checked for an
    /// upstream update. Built here because it needs the whole discovery result
    /// and the `npx skills` lockfile, both of which are read on this thread.
    pub targets: Vec<UpdateTarget>,
}

impl Scan {
    /// Walks every scope under `roots` and maps the result.
    ///
    /// Blocking, and the only function in the application that reads the
    /// filesystem for a list of skills. Call it from a background task.
    pub fn load(roots: &Roots) -> Self {
        let result = Discovery::new(roots.clone()).run();

        let agent_counts: HashMap<&'static str, usize> =
            result.counts_by_agent().into_iter().collect();
        let installed = Registry::all()
            .iter()
            .filter(|agent| !agent.is_shared())
            .filter(|agent| roots.agent_dir(agent).is_dir())
            .collect::<Vec<_>>();

        // The store is `~/.agents/skills`, which the shared row below already
        // lists. What is worth a row of its own is the private directory,
        // because it is the one place a skill can be that no agent reads.
        let private = roots.private_dir();
        let mut dirs = vec![DirStatus {
            id: PRIVATE_ID,
            label: "Hidden skills".into(),
            exists: private.is_dir(),
            path: private,
        }];
        dirs.extend(Registry::all().iter().map(|agent| {
            let path = roots.agent_dir(agent);
            DirStatus {
                id: agent.id,
                label: agent.display_name.into(),
                exists: path.is_dir(),
                path,
            }
        }));

        // Read but never written: `npx skills` owns this file, and co-owning
        // it would invite two tools to race over it.
        let lock = SkillLock::read(roots);
        let targets = skillbase_core::update_targets(&result.skills, &lock);

        let skills: Vec<SkillView> = result
            .skills
            .iter()
            .map(|skill| SkillView::from_discovered(skill, &lock))
            .collect();

        // Every group but Updates can be counted from the disk this thread has
        // just read. Updates depends on an answer GitHub has not been asked for
        // yet, so its slot stays at zero and [`Scan::count`] works it out from
        // the check instead.
        let mut library_counts = [0usize; Library::ALL.len()];
        for library in Library::ALL {
            library_counts[library.index()] = skills
                .iter()
                .filter(|s| library.matches(s, &|_| false))
                .count();
        }

        Self {
            skills,
            warnings: result.warnings.iter().map(SharedString::from).collect(),
            installed,
            dirs,
            agent_counts,
            library_counts,
            targets,
        }
    }

    /// The skill with this name, if the last scan found it.
    pub fn get(&self, name: &str) -> Option<&SkillView> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    /// How many skills a sidebar row lists.
    ///
    /// `has_update` is the update check's answer for one skill; see
    /// [`Library::matches`] for why it is not on the scan.
    pub fn count(&self, scope: Scope, has_update: impl Fn(&str) -> bool) -> usize {
        match scope {
            // The one row whose membership the scan could not settle.
            Scope::Library(Library::Updates) => self
                .skills
                .iter()
                .filter(|skill| has_update(&skill.name))
                .count(),
            Scope::Library(library) => self.library_counts[library.index()],
            Scope::Agent(id) => self.agent_counts.get(id).copied().unwrap_or(0),
        }
    }

    /// The skills a scope shows, narrowed by the search query.
    pub fn filter(
        &self,
        scope: Scope,
        query: &str,
        has_update: impl Fn(&str) -> bool,
    ) -> Vec<&SkillView> {
        self.skills
            .iter()
            .filter(|skill| scope.shows_with(skill, &has_update) && skill.matches_query(query))
            .collect()
    }
}

impl SkillView {
    fn from_discovered(skill: &DiscoveredSkill, lock: &SkillLock) -> Self {
        let description = skill.description().unwrap_or_default();
        // A description is one line in the list, so a wrapped YAML scalar has
        // to lose its newlines before it gets there.
        let description: String = description.split_whitespace().collect::<Vec<_>>().join(" ");

        let issues = skill
            .doc
            .as_ref()
            .map(|doc| {
                doc.validate()
                    .into_iter()
                    .map(|issue| Issue {
                        field: issue.field,
                        is_error: issue.is_error(),
                        message: issue.error.to_string().into(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        Self {
            name: skill.name.clone().into(),
            description: description.into(),
            origin: skill.origin.clone(),
            managed: skill.managed,
            parse_error: skill
                .parse_error
                .as_ref()
                .map(|e| SharedString::from(e.to_string())),
            issues,
            conflicts: skill.conflicts.clone(),
            locations: skill.locations.clone(),
            codex_disabled: skill.codex_disabled,
            visible_to: skill.visible_to(),
            in_shared: skill.is_present_in(SHARED_ID),
            provenance: provenance_of(skill, lock),
        }
    }

    /// Rebuilds the discovery record a delete plan is computed from.
    ///
    /// [`skillbase_core::Installer::plan_delete`] reads only the origin and the
    /// locations, both of which this view carries, so a plan can be recomputed
    /// without keeping the whole scan alive.
    pub fn as_discovered(&self) -> DiscoveredSkill {
        DiscoveredSkill {
            name: self.name.to_string(),
            origin: self.origin.clone(),
            locations: self.locations.clone(),
            doc: None,
            parse_error: None,
            conflicts: self.conflicts.clone(),
            managed: self.managed,
            codex_disabled: self.codex_disabled,
        }
    }
}

/// Abbreviates a path under the home directory to `~/…`, so the interface can
/// name a location without a 90-character prefix.
pub fn display_path(path: &Path, roots: &Roots) -> SharedString {
    match path.strip_prefix(roots.home()) {
        Ok(rest) => format!("~/{}", rest.display()).into(),
        Err(_) => path.display().to_string().into(),
    }
}

/// A list of names as English reads one: "A", "A and B", "A, B and C".
///
/// Used where the names are part of a sentence rather than a column, so a bare
/// comma-separated list would read as a fragment.
pub fn join_and(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The first seven characters of a git sha, which is how git itself shortens
/// one and enough to recognise it against a commit page.
pub fn short_sha(sha: &str) -> SharedString {
    let short: String = sha.chars().take(SHORT_SHA_LEN).collect();
    short.into()
}

/// How many characters of a sha the interface shows.
const SHORT_SHA_LEN: usize = 7;

/// Seconds since the Unix epoch, or 0 before it.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// How long ago a Unix timestamp was, in words.
///
/// Rounded down to the coarsest unit that still has a whole number in it,
/// because "checked 2 hours ago" is what the reader wants to know and "7412
/// seconds" is not. A timestamp in the future reads as "just now": a clock that
/// has been moved is not worth a sentence of its own.
pub fn ago(unix: i64) -> SharedString {
    if unix <= 0 {
        return "never".into();
    }
    let seconds = now_unix().saturating_sub(unix).max(0) as u64;
    // "just now" is already past tense. A counted duration is not, so it is the
    // one that needs the suffix: "checked 1 minute" is not English.
    match counted_duration(seconds) {
        Some(counted) => format!("{counted} ago").into(),
        None => "just now".into(),
    }
}

/// A number of seconds in words, for a wait the reader is being asked to sit
/// through. `zero` is what to say when there is nothing left to wait for.
pub fn in_words(seconds: u64) -> SharedString {
    match counted_duration(seconds) {
        Some(counted) => counted.into(),
        // Every caller reads "resets in {}", so the under-a-minute case has to
        // be a noun phrase that fits there. "any moment now" does not.
        None => "under a minute".into(),
    }
}

/// A duration as a count of the coarsest whole unit that fits, or `None` when
/// it is under a minute and there is no whole unit to name. The caller supplies
/// the wording for both cases, because "just now" and "any moment now" point in
/// opposite directions in time.
fn counted_duration(seconds: u64) -> Option<String> {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    let (count, unit) = match seconds {
        s if s < MINUTE => return None,
        s if s < HOUR => (s / MINUTE, "minute"),
        s if s < DAY => (s / HOUR, "hour"),
        s => (s / DAY, "day"),
    };
    Some(format!(
        "{count} {unit}{}",
        if count == 1 { "" } else { "s" }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duration_reads_as_the_coarsest_whole_unit() {
        assert_eq!(counted_duration(0), None);
        assert_eq!(counted_duration(59), None);
        assert_eq!(counted_duration(60).as_deref(), Some("1 minute"));
        assert_eq!(counted_duration(3599).as_deref(), Some("59 minutes"));
        assert_eq!(counted_duration(3600).as_deref(), Some("1 hour"));
        assert_eq!(counted_duration(86_400).as_deref(), Some("1 day"));
        assert_eq!(counted_duration(90_000).as_deref(), Some("1 day"));
    }

    #[test]
    fn a_timestamp_that_was_never_recorded_says_so() {
        assert_eq!(ago(0), "never");
        assert_eq!(ago(-1), "never");
    }

    #[test]
    fn a_past_time_reads_as_past_and_a_wait_does_not() {
        // The bug this guards: "checked 1 minute", with no "ago".
        assert_eq!(ago(now_unix() - 60), "1 minute ago");
        assert_eq!(ago(now_unix() - 7200), "2 hours ago");
        assert_eq!(ago(now_unix()), "just now");

        assert_eq!(in_words(60), "1 minute");
        assert_eq!(in_words(0), "under a minute");

        // Both callers read "resets in {}", so each phrasing must fit there.
        for seconds in [0, 30, 60, 7200] {
            let sentence = format!("It resets in {}.", in_words(seconds));
            assert!(!sentence.contains("in any"), "{sentence}");
        }
    }

    #[test]
    fn a_sha_is_shortened_the_way_git_shortens_one() {
        assert_eq!(
            short_sha("4f2a1c9d8e7b6a5f4e3d2c1b0a9f8e7d6c5b4a39"),
            "4f2a1c9"
        );
        assert_eq!(short_sha("abc"), "abc");
        assert_eq!(short_sha(""), "");
    }

    #[test]
    fn a_list_of_names_reads_as_a_sentence() {
        assert_eq!(join_and(&[]), "");
        assert_eq!(join_and(&["Claude Code"]), "Claude Code");
        assert_eq!(
            join_and(&["Claude Code", "GitHub Copilot"]),
            "Claude Code and GitHub Copilot"
        );
        assert_eq!(join_and(&["A", "B", "C"]), "A, B and C");
    }

    #[test]
    fn the_most_used_ordering_says_where_its_numbers_come_from() {
        assert!(SkillSort::Name.description().is_none());
        let said = SkillSort::MostUsed.description().expect("a description");
        // The two things a zero cannot say for itself: who was counted, and
        // how far back the count goes.
        assert!(said.contains("Claude Code"), "{said}");
        assert!(said.contains(&CLAUDE_RETENTION_DAYS.to_string()), "{said}");
    }

    #[test]
    fn the_updates_group_answers_from_the_check_and_not_from_the_disk() {
        let skill = SkillView {
            name: "pdf".into(),
            description: "".into(),
            origin: PathBuf::from("/store/pdf"),
            managed: true,
            parse_error: None,
            issues: Vec::new(),
            conflicts: Vec::new(),
            locations: Vec::new(),
            codex_disabled: false,
            visible_to: Vec::new(),
            in_shared: false,
            provenance: None,
        };

        assert!(Library::Updates.matches(&skill, &|_| true));
        assert!(!Library::Updates.matches(&skill, &|_| false));
        // Every other group ignores the answer it was handed.
        assert!(Library::Managed.matches(&skill, &|_| false));
        assert!(!Library::Unmanaged.matches(&skill, &|_| true));
    }

    #[test]
    fn a_scope_answers_about_updates_only_from_the_check() {
        // The bug this guards: the callers that decide whether a selection
        // survives a change of scope used to ask a version of this that
        // answered "yes" for every skill, so the detail pane kept a skill the
        // Updates list does not show.
        let skill = SkillView {
            name: "pdf".into(),
            description: "".into(),
            origin: PathBuf::from("/store/pdf"),
            managed: true,
            parse_error: None,
            issues: Vec::new(),
            conflicts: Vec::new(),
            locations: Vec::new(),
            codex_disabled: false,
            visible_to: Vec::new(),
            in_shared: false,
            provenance: None,
        };
        assert!(Scope::Library(Library::Updates).shows_with(&skill, &|_| true));
        assert!(!Scope::Library(Library::Updates).shows_with(&skill, &|_| false));
    }

    /// Every setting set to something other than its default, so a round trip
    /// that drops one is visible.
    fn arranged() -> Preferences {
        Preferences {
            show_all_agents: true,
            sort: SkillSort::MostUsed,
            scope: Scope::Library(Library::Unmanaged),
            selected: Some("pdf".to_string()),
            sidebar_collapsed: true,
            agents_collapsed: false,
            list_width: Some(412.5),
            window: Some(WindowFrame {
                x: 120.,
                y: 25.,
                width: 1100.,
                height: 700.,
            }),
            display: Some("37D8832A-2D66-02CA-B9F7-8F30A301B230".to_string()),
        }
    }

    #[test]
    fn the_settings_reader_understands_what_the_writer_writes() {
        let written = arranged().to_json();
        let read = Preferences::from_settings(&parse_settings(&written).expect("valid JSON"));
        assert_eq!(read, arranged());

        // And the defaults survive a round trip as themselves, rather than
        // coming back as whatever `Default` happens to be for the type.
        let written = Preferences::default().to_json();
        let read = Preferences::from_settings(&parse_settings(&written).expect("valid JSON"));
        assert_eq!(read, Preferences::default());
    }

    #[test]
    fn a_missing_or_malformed_setting_reads_as_no_preference() {
        // Each of these is valid JSON that says nothing this build can use, so
        // each key falls back to its own default rather than costing the
        // others.
        for text in [
            "{}",
            "{\"show_all_agents\": \"yes\"}",
            "{\"sort\": \"by-vibes\"}",
            "{\"scope\": \"library:nonesuch\"}",
            "{\"scope\": \"agent:no-such-agent\"}",
            "{\"other\": true}",
            "{\"window_x\": 10, \"window_y\": 10}",
            "{\"list_width\": \"wide\"}",
        ] {
            let settings = parse_settings(text).unwrap_or_else(|e| panic!("{text:?}: {e}"));
            assert_eq!(
                Preferences::from_settings(&settings),
                Preferences::default(),
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_file_that_is_not_json_is_a_problem_rather_than_a_silent_default() {
        // The bug this guards: reading with `unwrap_or_default`, so a file the
        // user hand-edited into invalid JSON reverted without a word and was
        // then overwritten by the next change of any setting.
        for text in ["not json at all", "{", "{\"a\":}", "{\"a\": 1} trailing"] {
            assert!(parse_settings(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn whitespace_around_the_colon_does_not_matter() {
        assert_eq!(
            parse_settings("{\"a\"   :   true}")
                .expect("valid JSON")
                .get("a"),
            Some(&Json::Bool(true))
        );
        assert_eq!(
            parse_settings("\n {\"a\":true}\n")
                .expect("valid JSON")
                .get("a"),
            Some(&Json::Bool(true))
        );
        assert_eq!(parse_settings("  {  }  ").expect("valid JSON").len(), 0);
    }

    #[test]
    fn a_key_is_read_as_a_key_and_not_as_a_substring_of_a_value() {
        // The bug this guards: the reader this replaced searched the whole
        // file for `"sort":`, so a skill name that quoted one found it.
        let settings =
            parse_settings("{\"selected\": \"a \\\"sort\\\": \\\"most-used\\\" skill\"}")
                .expect("valid JSON");
        assert_eq!(Preferences::from_settings(&settings).sort, SkillSort::Name);
        assert_eq!(
            settings.str_at("selected"),
            Some("a \"sort\": \"most-used\" skill")
        );
    }

    #[test]
    fn a_name_that_needs_escaping_survives_the_round_trip() {
        for name in ["quote\"inside", "back\\slash", "tab\there", "emoji-🙂"] {
            let prefs = Preferences {
                selected: Some(name.to_string()),
                ..Default::default()
            };
            let read =
                Preferences::from_settings(&parse_settings(&prefs.to_json()).expect("valid JSON"));
            assert_eq!(read.selected.as_deref(), Some(name));
        }
        // And an escape the writer never emits is still understood on the way
        // back in, because the file is meant to be hand-editable.
        let settings = parse_settings("{\"selected\": \"\\u0041\\uD83D\\uDE42\"}").unwrap();
        assert_eq!(settings.str_at("selected"), Some("A🙂"));
        assert!(parse_settings("{\"selected\": \"\\uD83D\"}").is_err());
    }

    /// A home directory of this test's own, removed when the test ends.
    struct TempHome(PathBuf);

    impl TempHome {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "skillbase-settings-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("a temp home");
            Self(path)
        }

        fn roots(&self) -> Roots {
            Roots::new(self.0.clone())
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Through the filesystem rather than through `to_json` alone, because the
    /// half of this that goes wrong is the writing: an empty file, a leftover
    /// temp file, a directory that was not there.
    #[test]
    fn the_preferences_come_back_off_the_disk_as_they_went_on() {
        let home = TempHome::new();
        let roots = home.roots();

        // Nothing saved yet is a first run, not a fault.
        let first = Preferences::load(&roots);
        assert_eq!(first.preferences, Preferences::default());
        assert!(first.problem.is_none());

        // And `~/.skillbase` is created on the way.
        let path = arranged().save(&roots).expect("a written file");
        assert_eq!(path, Preferences::path(&roots));

        let loaded = Preferences::load(&roots);
        assert_eq!(loaded.preferences, arranged());
        assert!(loaded.problem.is_none());

        // The temp file the write goes through is renamed, not left behind:
        // nothing else reads one or removes it.
        let mut left: Vec<String> = fs::read_dir(path.parent().expect("a parent"))
            .expect("the settings directory")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into()
            })
            .collect();
        left.sort();
        assert_eq!(left, ["settings.json"]);
    }

    #[test]
    fn a_file_that_is_empty_or_broken_is_reported_and_then_written_over() {
        // The first of these is what a truncated write used to leave: `fs::write`
        // empties the file before it writes it.
        for text in ["", "{\"sort\": ", "not json at all"] {
            let home = TempHome::new();
            let roots = home.roots();
            let path = Preferences::path(&roots);
            fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
            fs::write(&path, text).expect("the file");

            let loaded = Preferences::load(&roots);
            assert_eq!(loaded.preferences, Preferences::default(), "{text:?}");
            assert!(loaded.problem.is_some(), "{text:?}");

            arranged().save(&roots).expect("a written file");
            let loaded = Preferences::load(&roots);
            assert_eq!(loaded.preferences, arranged(), "{text:?}");
            assert!(loaded.problem.is_none(), "{text:?}");
        }
    }

    /// The usable area of the built-in display, in the shape the platform
    /// reports it: measured from the display's own top-left corner, with the
    /// menu bar's 25pt and the Dock's 62pt outside it. Every display reads this
    /// way, whatever its place on the desktop, which is why the frame is saved
    /// with the display it was measured against.
    const LAPTOP: WindowFrame = WindowFrame {
        x: 0.,
        y: 25.,
        width: 1512.,
        height: 895.,
    };
    /// A wider external monitor, reported the same way.
    const MONITOR: WindowFrame = WindowFrame {
        x: 0.,
        y: 25.,
        width: 2560.,
        height: 1350.,
    };
    const MIN: (f32, f32) = (860., 520.);

    #[test]
    fn a_window_that_still_fits_comes_back_where_it_was() {
        let saved = WindowFrame {
            x: 100.,
            y: 80.,
            width: 1200.,
            height: 800.,
        };
        assert_eq!(fit_to_display(saved, LAPTOP, MIN), Some(saved));
        assert_eq!(fit_to_display(saved, MONITOR, MIN), Some(saved));
    }

    #[test]
    fn a_window_from_a_display_that_is_gone_is_moved_onto_this_one() {
        // The bug this guards: saved at x=1500 on a 2560-wide monitor and
        // reopened with only the laptop attached, the window used to be placed
        // at x=1240 on a 1512-wide screen, hanging a thousand points off the
        // right-hand edge.
        let saved = WindowFrame {
            x: 1500.,
            y: 40.,
            width: 1320.,
            height: 860.,
        };
        let fitted = fit_to_display(saved, LAPTOP, MIN).expect("a usable frame");
        assert!(fitted.x >= LAPTOP.x, "{fitted:?}");
        assert!(fitted.y >= LAPTOP.y, "{fitted:?}");
        assert!(
            fitted.x + fitted.width <= LAPTOP.x + LAPTOP.width,
            "{fitted:?}"
        );
        assert!(
            fitted.y + fitted.height <= LAPTOP.y + LAPTOP.height,
            "{fitted:?}"
        );
        // The width the user chose still fits, so only the position moved.
        assert_eq!(fitted.width, 1320.);
    }

    #[test]
    fn a_window_larger_than_the_screen_shrinks_to_it() {
        let saved = WindowFrame {
            x: 0.,
            y: 0.,
            width: 3000.,
            height: 2000.,
        };
        let fitted = fit_to_display(saved, LAPTOP, MIN).expect("a usable frame");
        assert_eq!(fitted.width, LAPTOP.width);
        assert_eq!(fitted.height, LAPTOP.height);
        // Including down to the top-left corner of the *usable* area, which is
        // below the menu bar rather than at the top of the screen.
        assert_eq!((fitted.x, fitted.y), (LAPTOP.x, LAPTOP.y));
    }

    #[test]
    fn a_window_mostly_off_the_bottom_is_dragged_back_into_view() {
        // The title bar is what a window is moved by, so a frame whose top is
        // below the screen is one the user cannot recover.
        let saved = WindowFrame {
            x: 1400.,
            y: 900.,
            width: 1000.,
            height: 700.,
        };
        let fitted = fit_to_display(saved, LAPTOP, MIN).expect("a usable frame");
        assert_eq!(fitted.x + fitted.width, LAPTOP.x + LAPTOP.width);
        assert_eq!(fitted.y + fitted.height, LAPTOP.y + LAPTOP.height);
    }

    #[test]
    fn a_frame_that_is_not_a_frame_gets_no_answer() {
        for saved in [
            WindowFrame {
                x: 0.,
                y: 0.,
                width: 0.,
                height: 700.,
            },
            WindowFrame {
                x: 0.,
                y: 0.,
                width: -10.,
                height: 700.,
            },
            WindowFrame {
                x: f32::NAN,
                y: 0.,
                width: 900.,
                height: 700.,
            },
            WindowFrame {
                x: 0.,
                y: 0.,
                width: f32::INFINITY,
                height: 700.,
            },
        ] {
            assert_eq!(fit_to_display(saved, LAPTOP, MIN), None, "{saved:?}");
        }
        // And a display with no area is nothing to fit onto.
        let sane = WindowFrame {
            x: 0.,
            y: 0.,
            width: 900.,
            height: 700.,
        };
        assert_eq!(
            fit_to_display(
                sane,
                WindowFrame {
                    x: 0.,
                    y: 0.,
                    width: 0.,
                    height: 0.,
                },
                MIN
            ),
            None
        );
    }

    #[test]
    fn a_screen_smaller_than_the_minimum_still_gets_a_window_that_fits_it() {
        let tiny = WindowFrame {
            x: 0.,
            y: 0.,
            width: 640.,
            height: 400.,
        };
        let saved = WindowFrame {
            x: 0.,
            y: 0.,
            width: 1200.,
            height: 800.,
        };
        let fitted = fit_to_display(saved, tiny, MIN).expect("a usable frame");
        assert_eq!((fitted.width, fitted.height), (tiny.width, tiny.height));
    }

    #[test]
    fn a_scope_survives_the_settings_file_in_both_directions() {
        let agent = Registry::all()
            .iter()
            .find(|agent| !agent.is_shared())
            .expect("at least one agent");
        for scope in Library::ALL
            .map(Scope::Library)
            .into_iter()
            .chain([Scope::Agent(agent.id)])
        {
            assert_eq!(Scope::from_key(&scope.key()), Some(scope), "{scope:?}");
        }
        // The two namespaces do not collide, and an unknown row is no answer.
        assert_eq!(Scope::from_key("all"), None);
        assert_eq!(Scope::from_key("agent:library:all"), None);
        assert_eq!(Scope::from_key(""), None);
    }
}
