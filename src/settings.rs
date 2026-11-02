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

use crate::hexview::HexLayout;
use crate::save::{BomChoice, EolChoice};
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

/// Hex row widths the settings accept (roadmap item 13): one of
/// these, or 0 for fit-to-window (the shell computes the width
/// from the window size). Anything else loads as the default.
pub const HEX_COLS_FIT: u32 = 0;
pub const HEX_COLS_CHOICES: [u32; 5] = [8, 16, 24, 32, 64];
pub const DEFAULT_HEX_COLS: u32 = 16;

/// Hex group sizes the settings accept (roadmap item 14).
pub const HEX_GROUP_CHOICES: [u32; 4] = [1, 2, 4, 8];
pub const DEFAULT_HEX_GROUP: u32 = 1;

/// The stored words for the save line-ending choice (roadmap
/// item 31) and the save byte-order-mark choice (item 32).
pub const SAVE_EOL_KEEP: &str = "keep";
pub const SAVE_EOL_LF: &str = "lf";
pub const SAVE_EOL_CRLF: &str = "crlf";
pub const SAVE_BOM_KEEP: &str = "keep";
pub const SAVE_BOM_ADD: &str = "add";
pub const SAVE_BOM_REMOVE: &str = "remove";

/// The accepted `map_window_mb` values (slice R4c, roadmap
/// item 1): 0 = map the whole file (the default); any other stored
/// value loads as 0.
pub const MAP_WINDOW_CHOICES_MB: [u32; 5] = [0, 512, 1024, 2048, 4096];

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
    /// Whether the data inspector panel is docked open (roadmap
    /// item 15).
    pub inspector_open: bool,
    /// Hex bytes per row (roadmap item 13): one of
    /// [`HEX_COLS_CHOICES`], or [`HEX_COLS_FIT`] (0) for
    /// fit-to-window. Any other value loads as the default.
    pub hex_cols: u32,
    /// Hex grouping in bytes (roadmap item 14): one of
    /// [`HEX_GROUP_CHOICES`]; any other value loads as 1.
    pub hex_group: u32,
    /// Hex groups are displayed big-endian (roadmap item 14):
    /// each group's bytes are shown reversed, so the group reads
    /// as one big-endian number. Display only — file bytes never
    /// change order.
    pub hex_endian: bool,
    /// Follow Tail switch (roadmap item 22, "Ghost Tail"): while
    /// on, a clean document whose file only grew on disk adopts the
    /// growth silently instead of showing the external-change bar.
    pub follow_tail: bool,
    /// Soft word wrap for text mode (roadmap item 19): view-only —
    /// the file's bytes never change.
    pub word_wrap: bool,
    /// Whether the strings pane is docked open (strings pane).
    pub strings_open: bool,
    /// Strings pane minimum run length in characters: one of 4, 6
    /// or 8; anything else loads as the default 4.
    pub strings_min_len: u32,
    /// Number of top lines pinned by the freeze pane (freeze
    /// pane): 0 = off, otherwise 1..=50.
    pub freeze_lines: u32,
    /// Memory-mapping window for very large files, in megabytes
    /// (slice R4c, roadmap item 1): one of
    /// [`MAP_WINDOW_CHOICES_MB`]; 0 maps the whole file. When set,
    /// the shell's detection/EOL sampling and other whole-slice
    /// reads go through remapped views of this size instead of a
    /// whole-file mapping. Settings-file only in this slice (no
    /// menu entry) — see PARITY.
    pub map_window_mb: u32,
    /// Block-clone fast save (slice R4c, roadmap item 4): when
    /// true, saving a file-backed document first tries cloning the
    /// original file's extents into the staging file
    /// (FSCTL_DUPLICATE_EXTENTS_TO_FILE) and writing only the
    /// changed hunks; any failure falls back to the normal
    /// streamed save, noted in the status bar. Off by default.
    pub fast_save_clone: bool,
    /// Line-ending handling for saves (roadmap item 31): one of
    /// the `SAVE_EOL_*` words; anything else loads as "keep".
    pub save_eol: String,
    /// Byte-order-mark handling for saves (roadmap item 32): one
    /// of the `SAVE_BOM_*` words; anything else loads as "keep".
    pub save_bom: String,
    /// Last window placement `(x, y, width, height)` in physical pixels;
    /// the shell validates it against the connected monitors before
    /// applying it.
    pub window: Option<(i32, i32, i32, i32)>,
    /// Recently opened files, most recent first, at most
    /// [`MAX_RECENT`] entries, no duplicates.
    pub recent: Vec<String>,
    /// The open tabs at the last clean exit (roadmap item 39):
    /// the paths of the documents that had one, in tab order,
    /// reopened on launch. Documents without a path are not
    /// listed (unsaved work keeps the crash-recovery path); the
    /// list is normalised by `crate::tabs::normalise_session`.
    pub session_tabs: Vec<String>,
    /// The active tab's index within [`Settings::session_tabs`].
    pub session_active: usize,
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
            inspector_open: false,
            hex_cols: DEFAULT_HEX_COLS,
            hex_group: DEFAULT_HEX_GROUP,
            hex_endian: false,
            follow_tail: false,
            word_wrap: false,
            strings_open: false,
            strings_min_len: 4,
            freeze_lines: 0,
            map_window_mb: 0,
            fast_save_clone: false,
            save_eol: SAVE_EOL_KEEP.to_string(),
            save_bom: SAVE_BOM_KEEP.to_string(),
            window: None,
            recent: Vec::new(),
            session_tabs: Vec::new(),
            session_active: 0,
        }
    }
}

impl Settings {
    /// The save line-ending choice as the save pipeline's enum.
    pub fn save_eol_choice(&self) -> EolChoice {
        match self.save_eol.as_str() {
            SAVE_EOL_LF => EolChoice::Lf,
            SAVE_EOL_CRLF => EolChoice::Crlf,
            _ => EolChoice::Keep,
        }
    }

    /// The save byte-order-mark choice as the save pipeline's enum.
    pub fn save_bom_choice(&self) -> BomChoice {
        match self.save_bom.as_str() {
            SAVE_BOM_ADD => BomChoice::Add,
            SAVE_BOM_REMOVE => BomChoice::Remove,
            _ => BomChoice::Keep,
        }
    }

    /// The hex display layout these settings describe. `fit_width`
    /// is the shell-computed row width used when `hex_cols` is
    /// [`HEX_COLS_FIT`]; it is ignored for a fixed width.
    pub fn hex_layout(&self, fit_width: u64) -> HexLayout {
        HexLayout {
            bytes_per_row: if self.hex_cols == HEX_COLS_FIT {
                fit_width.max(1)
            } else {
                self.hex_cols as u64
            },
            group: self.hex_group as usize,
            big_endian: self.hex_endian,
        }
    }

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
        out.push_str(&format!("  \"inspector_open\": {},\n", self.inspector_open));
        out.push_str(&format!("  \"hex_cols\": {},\n", self.hex_cols));
        out.push_str(&format!("  \"hex_group\": {},\n", self.hex_group));
        out.push_str(&format!("  \"hex_endian\": {},\n", self.hex_endian));
        out.push_str(&format!("  \"follow_tail\": {},\n", self.follow_tail));
        out.push_str(&format!("  \"word_wrap\": {},\n", self.word_wrap));
        out.push_str(&format!("  \"strings_open\": {},\n", self.strings_open));
        out.push_str(&format!(
            "  \"strings_min_len\": {},\n",
            self.strings_min_len
        ));
        out.push_str(&format!("  \"freeze_lines\": {},\n", self.freeze_lines));
        out.push_str(&format!("  \"map_window_mb\": {},\n", self.map_window_mb));
        out.push_str(&format!(
            "  \"fast_save_clone\": {},\n",
            self.fast_save_clone
        ));
        out.push_str(&format!(
            "  \"save_eol\": \"{}\",\n",
            json_escape(&self.save_eol)
        ));
        out.push_str(&format!(
            "  \"save_bom\": \"{}\",\n",
            json_escape(&self.save_bom)
        ));
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
        out.push_str("],\n");
        out.push_str("  \"session_tabs\": [");
        for (i, path) in self.session_tabs.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&format!("\"{}\"", json_escape(path)));
        }
        out.push_str("],\n");
        out.push_str(&format!("  \"session_active\": {}\n", self.session_active));
        out.push_str("}\n");
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
                "inspector_open" => settings.inspector_open = parser.boolean()?,
                "hex_cols" => {
                    let n = parser.integer()?;
                    settings.hex_cols = match n {
                        0 => HEX_COLS_FIT,
                        8 | 16 | 24 | 32 | 64 => n as u32,
                        _ => DEFAULT_HEX_COLS,
                    };
                }
                "hex_group" => {
                    let n = parser.integer()?;
                    settings.hex_group = match n {
                        1 | 2 | 4 | 8 => n as u32,
                        _ => DEFAULT_HEX_GROUP,
                    };
                }
                "hex_endian" => settings.hex_endian = parser.boolean()?,
                "follow_tail" => settings.follow_tail = parser.boolean()?,
                "word_wrap" => settings.word_wrap = parser.boolean()?,
                "strings_open" => settings.strings_open = parser.boolean()?,
                "strings_min_len" => {
                    let n = parser.integer()?;
                    settings.strings_min_len = match n {
                        4 | 6 | 8 => n as u32,
                        _ => 4,
                    };
                }
                "freeze_lines" => {
                    let n = parser.integer()?;
                    settings.freeze_lines = (n.clamp(0, 50)) as u32;
                }
                "map_window_mb" => {
                    let n = parser.integer()?;
                    settings.map_window_mb = match n {
                        0 | 512 | 1024 | 2048 | 4096 => n as u32,
                        _ => 0, // anything else means whole-file mapping
                    };
                }
                "fast_save_clone" => settings.fast_save_clone = parser.boolean()?,
                "save_eol" => {
                    let word = parser.string()?;
                    settings.save_eol = match word.as_str() {
                        SAVE_EOL_LF | SAVE_EOL_CRLF => word,
                        _ => SAVE_EOL_KEEP.to_string(),
                    };
                }
                "save_bom" => {
                    let word = parser.string()?;
                    settings.save_bom = match word.as_str() {
                        SAVE_BOM_ADD | SAVE_BOM_REMOVE => word,
                        _ => SAVE_BOM_KEEP.to_string(),
                    };
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
                "session_tabs" => {
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
                    settings.session_tabs = paths;
                }
                "session_active" => {
                    let n = parser.integer()?;
                    settings.session_active = if n < 0 { 0 } else { n as usize };
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
        // The session list keeps duplicates (the same file may
        // legitimately be open in two tabs) but drops empties,
        // caps its length, and clamps the active index.
        let (tabs, active) =
            crate::tabs::normalise_session(settings.session_tabs, settings.session_active);
        settings.session_tabs = tabs;
        settings.session_active = active;
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
/// keys, string / integer / boolean / null / array values. It is deliberately not
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

    fn boolean(&mut self) -> Result<bool, String> {
        self.skip_ws();
        if self.chars[self.pos..].starts_with(&['t', 'r', 'u', 'e']) {
            self.pos += 4;
            Ok(true)
        } else if self.chars[self.pos..].starts_with(&['f', 'a', 'l', 's', 'e']) {
            self.pos += 5;
            Ok(false)
        } else {
            Err(format!("expected a boolean at character {}", self.pos))
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
            inspector_open: true,
            hex_cols: 32,
            hex_group: 4,
            hex_endian: true,
            follow_tail: true,
            word_wrap: true,
            strings_open: true,
            strings_min_len: 6,
            freeze_lines: 3,
            map_window_mb: 1024,
            fast_save_clone: true,
            save_eol: SAVE_EOL_CRLF.to_string(),
            save_bom: SAVE_BOM_ADD.to_string(),
            window: Some((-120, 40, 1100, 750)),
            recent: vec![
                "C:\\Logs\\server.log".to_string(),
                "D:\\data\\core.dmp".to_string(),
            ],
            session_tabs: vec![
                "C:\\Logs\\server.log".to_string(),
                "C:\\Logs\\server.log".to_string(),
                "D:\\data\\core.dmp".to_string(),
            ],
            session_active: 2,
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
        assert!(!s.inspector_open);
        assert_eq!(s.hex_cols, DEFAULT_HEX_COLS);
        assert_eq!(s.hex_group, DEFAULT_HEX_GROUP);
        assert!(!s.hex_endian);
        assert_eq!(s.save_eol, SAVE_EOL_KEEP);
        assert_eq!(s.save_bom, SAVE_BOM_KEEP);
        assert_eq!(s.window, None);
        assert!(s.recent.is_empty());
        assert!(s.session_tabs.is_empty());
        assert_eq!(s.session_active, 0);
    }

    #[test]
    fn session_list_round_trips_and_normalises() {
        // The sample's session list (with a deliberate duplicate —
        // the same file in two tabs) survives the round trip.
        let s = sample();
        let back = Settings::from_json(&s.to_json()).unwrap();
        assert_eq!(back.session_tabs, s.session_tabs);
        assert_eq!(back.session_active, 2);
        // Empties drop out, a wild active index clamps, a negative
        // one loads as 0.
        let s = Settings::from_json(r#"{"session_tabs":["","a.txt","b.txt"],"session_active":9}"#)
            .unwrap();
        assert_eq!(
            s.session_tabs,
            vec!["a.txt".to_string(), "b.txt".to_string()]
        );
        assert_eq!(s.session_active, 1);
        let s = Settings::from_json(r#"{"session_active":-3}"#).unwrap();
        assert_eq!(s.session_active, 0);
        assert!(s.session_tabs.is_empty());
    }

    #[test]
    fn view_and_save_keys_round_trip_and_validate() {
        // The sample sets every new key to a non-default value; the
        // generic round-trip test already proves they survive, so
        // here the enum/word conversions are pinned down.
        let s = sample();
        assert_eq!(s.save_eol_choice(), EolChoice::Crlf);
        assert_eq!(s.save_bom_choice(), BomChoice::Add);
        let layout = s.hex_layout(48);
        assert_eq!(layout.bytes_per_row, 32);
        assert_eq!(layout.group, 4);
        assert!(layout.big_endian);
        // Fit-to-window takes the shell-computed width instead.
        let fit = Settings {
            hex_cols: HEX_COLS_FIT,
            ..Settings::default()
        };
        assert_eq!(fit.hex_layout(48).bytes_per_row, 48);
        assert_eq!(fit.save_eol_choice(), EolChoice::Keep);
        assert_eq!(fit.save_bom_choice(), BomChoice::Keep);

        // Out-of-set values load as the documented defaults.
        let text = r#"{"hex_cols":12,"hex_group":3,"save_eol":"tabs","save_bom":"yes","hex_endian":true,"inspector_open":true}"#;
        let s = Settings::from_json(text).unwrap();
        assert_eq!(s.hex_cols, DEFAULT_HEX_COLS);
        assert_eq!(s.hex_group, DEFAULT_HEX_GROUP);
        assert_eq!(s.save_eol, SAVE_EOL_KEEP);
        assert_eq!(s.save_bom, SAVE_BOM_KEEP);
        assert!(s.hex_endian);
        assert!(s.inspector_open);
        // Every accepted value loads as itself, fit included.
        for cols in [0, 8, 16, 24, 32, 64] {
            let s = Settings::from_json(&format!("{{\"hex_cols\":{cols}}}")).unwrap();
            assert_eq!(s.hex_cols, cols);
        }
        for group in [1, 2, 4, 8] {
            let s = Settings::from_json(&format!("{{\"hex_group\":{group}}}")).unwrap();
            assert_eq!(s.hex_group, group);
        }
        let s = Settings::from_json(r#"{"save_eol":"lf","save_bom":"remove"}"#).unwrap();
        assert_eq!(s.save_eol_choice(), EolChoice::Lf);
        assert_eq!(s.save_bom_choice(), BomChoice::Remove);
    }

    #[test]
    fn r4c_keys_round_trip_and_validate() {
        // The sample sets both new keys to non-default values; the
        // generic round-trip test proves they survive. Here the
        // map-window choice validation is pinned down.
        let s = sample();
        assert_eq!(s.map_window_mb, 1024);
        assert!(s.fast_save_clone);
        for mb in [0, 512, 1024, 2048, 4096] {
            let s = Settings::from_json(&format!("{{\"map_window_mb\":{mb}}}")).unwrap();
            assert_eq!(s.map_window_mb, mb);
        }
        for mb in [1, 256, 8192, -512] {
            let s = Settings::from_json(&format!("{{\"map_window_mb\":{mb}}}")).unwrap();
            assert_eq!(s.map_window_mb, 0, "map_window_mb {mb} must load as 0");
        }
        let s = Settings::from_json(r#"{"fast_save_clone":true}"#).unwrap();
        assert!(s.fast_save_clone);
        assert_eq!(Settings::default().map_window_mb, 0);
        assert!(!Settings::default().fast_save_clone);
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
