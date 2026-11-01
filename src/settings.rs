//! Portable program settings (SPEC.md section 6.2).
//!
//! Settings live in exactly one place: `settings.json` inside the
//! `data/` folder next to the program file. Nothing is written to
//! `%APPDATA%`, `%LOCALAPPDATA%`, `%PROGRAMDATA%`, or the registry —
//! ever.
//!
//! The format is hand-rolled JSON (the project is zero-dependency, no
//! serde): the writer emits one small object, and the reader is a small
//! tolerant parser for exactly that shape. A file that cannot be parsed
//! is never a crash and never silently dropped: the program falls back
//! to defaults and the bad file is renamed to `settings.json.bad` next
//! to the original, so the user (or a bug report) can see what was
//! there.

use std::path::{Path, PathBuf};

use crate::theme;

/// The settings file's name inside the `data/` folder.
pub const SETTINGS_FILE_NAME: &str = "settings.json";

/// Maximum number of recent files kept.
pub const MAX_RECENT: usize = 8;

/// Font size bounds (points, scaled by DPI at render time).
pub const MIN_FONT_SIZE: u16 = 8;
pub const MAX_FONT_SIZE: u16 = 48;
pub const DEFAULT_FONT_SIZE: u16 = 14;

/// Undo bounds (roadmap item 6): the history keeps at most this many
/// steps and at most this many megabytes of stored deltas, whichever
/// runs out first. Both are owner-settable in `settings.json`.
pub const MIN_UNDO_LIMIT: usize = 16;
pub const MAX_UNDO_LIMIT: usize = 65_536;
pub const DEFAULT_UNDO_LIMIT: usize = 1024;
pub const MIN_UNDO_BYTE_BUDGET_MB: u64 = 1;
pub const MAX_UNDO_BYTE_BUDGET_MB: u64 = 4096;
pub const DEFAULT_UNDO_BYTE_BUDGET_MB: u64 = 256;

/// Column ruler bounds: 0 = off; any other value is the character
/// column the guideline is drawn at (roadmap item 21).
pub const MAX_RULER_COLUMN: u32 = 4096;
pub const DEFAULT_RULER_COLUMN: u32 = 0;

/// The folder that holds all portable program state: `<exe-dir>/data`.
/// Falls back to a `data/` folder relative to the working directory
/// when the executable path cannot be determined (tests, odd launchers)
/// — the fallback is documented here rather than hidden.
pub fn data_dir() -> PathBuf {
    match std::env::current_exe() {
        Ok(mut exe) => {
            exe.pop();
            exe.join("data")
        }
        Err(_) => PathBuf::from("data"),
    }
}

/// The full path of the settings file inside [`data_dir`].
pub fn settings_path() -> PathBuf {
    data_dir().join(SETTINGS_FILE_NAME)
}

/// Escape a string for inclusion in JSON output (quotes, backslash, and
/// control characters). Shared with the recovery metadata writer in the
/// GUI shell.
pub fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out
}

/// Program settings, persisted as one JSON object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Built-in theme name (see `crate::theme`). Unknown names resolve
    /// to the default theme at render time.
    pub theme: String,
    /// Owner-chosen accent as a `#RRGGBB` hex string, or `None` for
    /// "theme default".
    pub accent_override: Option<String>,
    /// Editor font size in points (clamped to
    /// [`MIN_FONT_SIZE`]..=[`MAX_FONT_SIZE`] on load).
    pub font_size: u16,
    /// Maximum number of undo steps kept (clamped to
    /// [`MIN_UNDO_LIMIT`]..=[`MAX_UNDO_LIMIT`] on load).
    pub undo_limit: usize,
    /// Maximum total size of the stored undo deltas, in megabytes
    /// (clamped to [`MIN_UNDO_BYTE_BUDGET_MB`]..=
    /// [`MAX_UNDO_BYTE_BUDGET_MB`] on load).
    pub undo_byte_budget_mb: u64,
    /// Character column of the optional vertical guideline in text
    /// mode; 0 = off (clamped to at most [`MAX_RULER_COLUMN`] on load).
    pub ruler_column: u32,
    /// Last window placement `(x, y, width, height)` in physical pixels;
    /// the shell validates it against the connected monitors before
    /// applying it.
    pub window: Option<(i32, i32, i32, i32)>,
    /// Recently opened files, most recent first, at most
    /// [`MAX_RECENT`] entries, no duplicates.
    pub recent: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: theme::DEFAULT_THEME_NAME.to_string(),
            accent_override: None,
            font_size: DEFAULT_FONT_SIZE,
            undo_limit: DEFAULT_UNDO_LIMIT,
            undo_byte_budget_mb: DEFAULT_UNDO_BYTE_BUDGET_MB,
            ruler_column: DEFAULT_RULER_COLUMN,
            window: None,
            recent: Vec::new(),
        }
    }
}

impl Settings {
    /// Record `path` as the most recently opened file: moved to the
    /// front, duplicates removed (compared case-insensitively, as
    /// Windows paths are), the list truncated to [`MAX_RECENT`].
    pub fn add_recent(&mut self, path: &str) {
        self.recent
            .retain(|existing| !existing.eq_ignore_ascii_case(path));
        self.recent.insert(0, path.to_string());
        self.recent.truncate(MAX_RECENT);
    }

    /// Serialise to the JSON text written to disk.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\n");
        out.push_str(&format!("  \"theme\": \"{}\",\n", json_escape(&self.theme)));
        match &self.accent_override {
            Some(hex) => out.push_str(&format!(
                "  \"accent_override\": \"{}\",\n",
                json_escape(hex)
            )),
            None => out.push_str("  \"accent_override\": null,\n"),
        }
        out.push_str(&format!("  \"font_size\": {},\n", self.font_size));
        out.push_str(&format!("  \"undo_limit\": {},\n", self.undo_limit));
        out.push_str(&format!(
            "  \"undo_byte_budget_mb\": {},\n",
            self.undo_byte_budget_mb
        ));
        out.push_str(&format!("  \"ruler_column\": {},\n", self.ruler_column));
        match self.window {
            Some((x, y, w, h)) => out.push_str(&format!("  \"window\": [{x}, {y}, {w}, {h}],\n")),
            None => out.push_str("  \"window\": null,\n"),
        }
        out.push_str("  \"recent\": [");
        for (i, path) in self.recent.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&format!("\"{}\"", json_escape(path)));
        }
        out.push_str("]\n}\n");
        out
    }

    /// Parse settings from JSON text. Any structural problem is an
    /// error; missing keys fall back to their defaults, and values are
    /// validated/clamped (font size, recent-list length and duplicates,
    /// accent must be a parseable hex colour or it is dropped).
    pub fn from_json(text: &str) -> Result<Settings, String> {
        let mut parser = Parser {
            chars: text.chars().collect(),
            pos: 0,
        };
        parser.skip_ws();
        parser.expect('{')?;
        let mut settings = Settings::default();
        loop {
            parser.skip_ws();
            if parser.eat('}') {
                break;
            }
            let key = parser.string()?;
            parser.skip_ws();
            parser.expect(':')?;
            parser.skip_ws();
            match key.as_str() {
                "theme" => settings.theme = parser.string()?,
                "accent_override" => {
                    if parser.eat_null() {
                        settings.accent_override = None;
                    } else {
                        let hex = parser.string()?;
                        // Only a parseable colour may be stored; a bad
                        // value is dropped, not kept to fail later.
                        settings.accent_override = theme::Rgba::from_hex(&hex).map(|c| c.to_hex());
                    }
                }
                "font_size" => {
                    let n = parser.integer()?;
                    settings.font_size =
                        (n.clamp(MIN_FONT_SIZE as i64, MAX_FONT_SIZE as i64)) as u16;
                }
                "undo_limit" => {
                    let n = parser.integer()?;
                    settings.undo_limit =
                        (n.clamp(MIN_UNDO_LIMIT as i64, MAX_UNDO_LIMIT as i64)) as usize;
                }
                "undo_byte_budget_mb" => {
                    let n = parser.integer()?;
                    settings.undo_byte_budget_mb = (n.clamp(
                        MIN_UNDO_BYTE_BUDGET_MB as i64,
                        MAX_UNDO_BYTE_BUDGET_MB as i64,
                    )) as u64;
                }
                "ruler_column" => {
                    let n = parser.integer()?;
                    settings.ruler_column = (n.clamp(0, MAX_RULER_COLUMN as i64)) as u32;
                }
                "window" => {
                    if parser.eat_null() {
                        settings.window = None;
                    } else {
                        parser.expect('[')?;
                        let mut nums = Vec::new();
                        loop {
                            parser.skip_ws();
                            if parser.eat(']') {
                                break;
                            }
                            nums.push(parser.integer()?);
                            parser.skip_ws();
                            parser.eat(',');
                        }
                        if nums.len() == 4 {
                            settings.window = Some((
                                nums[0] as i32,
                                nums[1] as i32,
                                nums[2] as i32,
                                nums[3] as i32,
                            ));
                        } else {
                            settings.window = None;
                        }
                    }
                }
                "recent" => {
                    parser.expect('[')?;
                    let mut paths = Vec::new();
                    loop {
                        parser.skip_ws();
                        if parser.eat(']') {
                            break;
                        }
                        paths.push(parser.string()?);
                        parser.skip_ws();
                        parser.eat(',');
                    }
                    settings.recent = paths;
                }
                _ => parser.skip_value()?,
            }
            parser.skip_ws();
            parser.eat(',');
        }
        // Normalise the recent list to the stored invariants.
        let mut deduped: Vec<String> = Vec::new();
        for path in settings.recent.drain(..) {
            if !deduped.iter().any(|p| p.eq_ignore_ascii_case(&path)) {
                deduped.push(path);
            }
        }
        deduped.truncate(MAX_RECENT);
        settings.recent = deduped;
        Ok(settings)
    }

    /// Load settings from `path`.
    ///
    /// * Missing file → defaults (first run is not an error).
    /// * Unreadable or corrupt file → defaults, and when the file exists
    ///   but cannot be parsed it is renamed to `<path>.bad` first, so
    ///   the evidence survives. A failed rename never blocks startup.
    pub fn load_from(path: &Path) -> Settings {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(_) => return Settings::default(),
        };
        match Self::from_json(&text) {
            Ok(settings) => settings,
            Err(_) => {
                let bad = path.with_extension("json.bad");
                let _ = std::fs::rename(path, &bad);
                Settings::default()
            }
        }
    }

    /// Save settings to `path`, creating the parent folder when needed.
    /// The write goes through a sibling temp file renamed over the
    /// target, so a crash mid-save leaves either the old or the new
    /// settings, never a torn file.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, self.to_json())?;
        match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Windows rename does not replace an existing file via
                // std::fs::rename on all targets; fall back to a direct
                // write so saving settings never hard-fails on that.
                let _ = std::fs::remove_file(&tmp);
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    std::fs::write(path, self.to_json())
                } else {
                    Err(e)
                }
            }
        }
    }
}

/// A minimal JSON reader for the settings shape: objects with string
/// keys, string / integer / null / array values. It is deliberately not
/// a general JSON parser — it reads what [`Settings::to_json`] writes,
/// tolerates whitespace and key order, and rejects everything else.
struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek();
        if ch.is_some() {
            self.pos += 1;
        }
        ch
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(ch) if ch.is_whitespace()) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, want: char) -> bool {
        self.skip_ws();
        if self.peek() == Some(want) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, want: char) -> Result<(), String> {
        if self.eat(want) {
            Ok(())
        } else {
            Err(format!("expected '{want}' at character {}", self.pos))
        }
    }

    fn eat_null(&mut self) -> bool {
        if self.chars[self.pos..].starts_with(&['n', 'u', 'l', 'l']) {
            self.pos += 4;
            true
        } else {
            false
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.skip_ws();
        self.expect('"')?;
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err("unterminated string".to_string()),
                Some('"') => return Ok(out),
                Some('\\') => match self.bump() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('/') => out.push('/'),
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    Some('t') => out.push('\t'),
                    Some('b') => out.push('\u{0008}'),
                    Some('f') => out.push('\u{000C}'),
                    Some('u') => {
                        let mut value: u32 = 0;
                        for _ in 0..4 {
                            let ch = self.bump().ok_or_else(|| "short \\u escape".to_string())?;
                            let digit = ch
                                .to_digit(16)
                                .ok_or_else(|| "bad \\u escape".to_string())?;
                            value = value * 16 + digit;
                        }
                        // Lone surrogates and out-of-range values become
                        // U+FFFD rather than an error: a settings file
                        // with a mangled path degrades, it does not brick
                        // the program.
                        out.push(char::from_u32(value).unwrap_or('\u{FFFD}'));
                    }
                    _ => return Err("unknown escape".to_string()),
                },
                Some(ch) => out.push(ch),
            }
        }
    }

    fn integer(&mut self) -> Result<i64, String> {
        self.skip_ws();
        let start = self.pos;
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(ch) if ch.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.pos == start || (self.pos == start + 1 && self.chars[start] == '-') {
            return Err(format!("expected a number at character {start}"));
        }
        self.chars[start..self.pos]
            .iter()
            .collect::<String>()
            .parse::<i64>()
            .map_err(|e| e.to_string())
    }

    /// Skip one value of any shape (used for keys this version does not
    /// know, so a newer settings file stays readable).
    fn skip_value(&mut self) -> Result<(), String> {
        self.skip_ws();
        match self.peek() {
            Some('"') => {
                self.string()?;
            }
            Some('{') => {
                self.pos += 1;
                loop {
                    self.skip_ws();
                    if self.eat('}') {
                        break;
                    }
                    self.string()?;
                    self.skip_ws();
                    self.expect(':')?;
                    self.skip_value()?;
                    self.skip_ws();
                    self.eat(',');
                }
            }
            Some('[') => {
                self.pos += 1;
                loop {
                    self.skip_ws();
                    if self.eat(']') {
                        break;
                    }
                    self.skip_value()?;
                    self.skip_ws();
                    self.eat(',');
                }
            }
            Some('t') => {
                if self.chars[self.pos..].starts_with(&['t', 'r', 'u', 'e']) {
                    self.pos += 4;
                } else {
                    return Err("bad literal".to_string());
                }
            }
            Some('f') => {
                if self.chars[self.pos..].starts_with(&['f', 'a', 'l', 's', 'e']) {
                    self.pos += 5;
                } else {
                    return Err("bad literal".to_string());
                }
            }
            Some('n') => {
                if !self.eat_null() {
                    return Err("bad literal".to_string());
                }
            }
            Some(_) => {
                self.integer()?;
            }
            None => return Err("unexpected end of input".to_string()),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Settings {
        Settings {
            theme: "Monokai".to_string(),
            accent_override: Some("#2F7CF6".to_string()),
            font_size: 18,
            undo_limit: 2048,
            undo_byte_budget_mb: 128,
            ruler_column: 80,
            window: Some((-120, 40, 1100, 750)),
            recent: vec![
                "C:\\Logs\\server.log".to_string(),
                "D:\\data\\core.dmp".to_string(),
            ],
        }
    }

    #[test]
    fn defaults_are_the_documented_ones() {
        let s = Settings::default();
        assert_eq!(s.theme, "Obsidian");
        assert_eq!(s.accent_override, None);
        assert_eq!(s.font_size, 14);
        assert_eq!(s.undo_limit, DEFAULT_UNDO_LIMIT);
        assert_eq!(s.undo_byte_budget_mb, DEFAULT_UNDO_BYTE_BUDGET_MB);
        assert_eq!(s.ruler_column, DEFAULT_RULER_COLUMN);
        assert_eq!(s.window, None);
        assert!(s.recent.is_empty());
    }

    #[test]
    fn json_round_trip_preserves_everything() {
        let s = sample();
        let text = s.to_json();
        let back = Settings::from_json(&text).unwrap();
        assert_eq!(back, s);
        // Windows paths with backslashes survive the round trip.
        assert_eq!(back.recent[0], "C:\\Logs\\server.log");
    }

    #[test]
    fn json_round_trip_with_nulls_and_empty_recent() {
        let s = Settings::default();
        let back = Settings::from_json(&s.to_json()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn save_and_load_through_a_real_file() {
        let dir =
            std::env::temp_dir().join(format!("veloedit-settings-test-{}", std::process::id()));
        let path = dir.join("nested").join(SETTINGS_FILE_NAME);
        let s = sample();
        s.save_to(&path).unwrap();
        assert!(path.exists());
        assert_eq!(Settings::load_from(&path), s);
        // Saving again over an existing file also works.
        let mut changed = s.clone();
        changed.font_size = 20;
        changed.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path).font_size, 20);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_file_loads_defaults() {
        let path = std::env::temp_dir().join("veloedit-no-such-settings-xyz.json");
        assert_eq!(Settings::load_from(&path), Settings::default());
    }

    #[test]
    fn corrupt_file_loads_defaults_and_is_renamed_bad() {
        let dir =
            std::env::temp_dir().join(format!("veloedit-settings-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SETTINGS_FILE_NAME);
        std::fs::write(&path, b"{this is not json at all").unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded, Settings::default());
        // The bad file is preserved under the .bad name, not deleted.
        let bad = dir.join("settings.json.bad");
        assert!(bad.exists(), "corrupt settings must be renamed to .bad");
        assert_eq!(std::fs::read(&bad).unwrap(), b"{this is not json at all");
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn values_are_validated_on_load() {
        let text = r#"{"theme":"Dracula","accent_override":"not-a-colour","font_size":900,"recent":["a","A","b","a"]}"#;
        let s = Settings::from_json(text).unwrap();
        assert_eq!(s.theme, "Dracula");
        assert_eq!(s.accent_override, None, "unparseable accent is dropped");
        assert_eq!(s.font_size, MAX_FONT_SIZE, "font size clamps to the max");
        // Case-insensitive dedupe, first occurrence wins, order preserved.
        assert_eq!(s.recent, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn undo_and_ruler_values_are_clamped() {
        // Below the floor, above the ceiling, and a good value.
        let text = r#"{"undo_limit":2,"undo_byte_budget_mb":99999,"ruler_column":120}"#;
        let s = Settings::from_json(text).unwrap();
        assert_eq!(s.undo_limit, MIN_UNDO_LIMIT);
        assert_eq!(s.undo_byte_budget_mb, MAX_UNDO_BYTE_BUDGET_MB);
        assert_eq!(s.ruler_column, 120);
        let text = r#"{"undo_limit":700000,"undo_byte_budget_mb":0,"ruler_column":-5}"#;
        let s = Settings::from_json(text).unwrap();
        assert_eq!(s.undo_limit, MAX_UNDO_LIMIT);
        assert_eq!(s.undo_byte_budget_mb, MIN_UNDO_BYTE_BUDGET_MB);
        assert_eq!(s.ruler_column, 0);
    }

    #[test]
    fn accent_is_normalised_on_load() {
        let text = r##"{"accent_override":"#2f7cf6"}"##;
        let s = Settings::from_json(text).unwrap();
        assert_eq!(s.accent_override, Some("#2F7CF6".to_string()));
    }

    #[test]
    fn unknown_keys_are_tolerated() {
        let text = r#"{"theme":"Zenburn","future":{"nested":[1,2,{"x":true}]},"font_size":12}"#;
        let s = Settings::from_json(text).unwrap();
        assert_eq!(s.theme, "Zenburn");
        assert_eq!(s.font_size, 12);
    }

    #[test]
    fn recent_list_is_capped_and_deduped() {
        let mut s = Settings::default();
        for i in 0..12 {
            s.add_recent(&format!("file{i}.txt"));
        }
        assert_eq!(s.recent.len(), MAX_RECENT);
        assert_eq!(s.recent[0], "file11.txt");
        s.add_recent("file5.txt");
        assert_eq!(s.recent[0], "file5.txt");
        assert_eq!(s.recent.len(), MAX_RECENT);
        assert_eq!(
            s.recent
                .iter()
                .filter(|p| p.as_str() == "file5.txt")
                .count(),
            1
        );
    }

    #[test]
    fn data_dir_is_named_data() {
        let dir = data_dir();
        assert_eq!(
            dir.file_name().map(|n| n.to_string_lossy().into_owned()),
            Some("data".to_string())
        );
        assert_eq!(
            settings_path()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned()),
            Some(SETTINGS_FILE_NAME.to_string())
        );
    }

    #[test]
    fn json_escape_covers_the_specials() {
        assert_eq!(json_escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
        assert_eq!(json_escape("plain"), "plain");
    }
}
