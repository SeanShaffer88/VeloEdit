//! Rebindable keyboard shortcuts (roadmap item 48, slice R4c).
//!
//! The shortcuts live in `data/keybindings.json` next to the
//! program: one flat JSON object mapping an action name (`"find"`,
//! `"save"`, `"toggle_hex"`, …) to a chord string (`"Ctrl+Shift+F"`).
//! This module is the pure core — the action registry, the chord
//! parser, and the merge of a user file over the built-in defaults.
//! The window shell translates between chords and Win32 virtual-key
//! codes and dispatches through the merged map.
//!
//! Merge rules: an action the file does not mention keeps its
//! default; a chord string replaces it; an empty string (or
//! `"none"`) unbinds the action. Anything the file gets wrong — an
//! unknown action, an unreadable chord, a file that is not a flat
//! string object at all — is collected as a plain-words problem and
//! the defaults stand; the shell reports the problems once at
//! startup ("keybindings.json had problems; defaults used") and
//! otherwise carries on. A missing file is not a problem: it just
//! means defaults.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The keybindings file's name inside the portable data folder.
pub const KEYBINDINGS_FILE_NAME: &str = "keybindings.json";

/// The full path of the keybindings file inside the portable
/// `data/` folder (see [`crate::settings::data_dir`]).
pub fn keybindings_path() -> PathBuf {
    crate::settings::data_dir().join(KEYBINDINGS_FILE_NAME)
}

/// A non-character key a chord can name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum NamedKey {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    Insert,
    Escape,
    Enter,
    Space,
    Tab,
}

impl NamedKey {
    /// The canonical spelling in chord strings and menu labels.
    pub fn name(self) -> &'static str {
        match self {
            NamedKey::Left => "Left",
            NamedKey::Right => "Right",
            NamedKey::Up => "Up",
            NamedKey::Down => "Down",
            NamedKey::Home => "Home",
            NamedKey::End => "End",
            NamedKey::PageUp => "PageUp",
            NamedKey::PageDown => "PageDown",
            NamedKey::Delete => "Delete",
            NamedKey::Insert => "Insert",
            NamedKey::Escape => "Escape",
            NamedKey::Enter => "Enter",
            NamedKey::Space => "Space",
            NamedKey::Tab => "Tab",
        }
    }

    /// Parse a chord string's key token as a named key
    /// (case-insensitive; a few common aliases accepted).
    fn parse(token: &str) -> Option<NamedKey> {
        let lower = token.to_ascii_lowercase();
        Some(match lower.as_str() {
            "left" => NamedKey::Left,
            "right" => NamedKey::Right,
            "up" => NamedKey::Up,
            "down" => NamedKey::Down,
            "home" => NamedKey::Home,
            "end" => NamedKey::End,
            "pageup" => NamedKey::PageUp,
            "pagedown" => NamedKey::PageDown,
            "delete" | "del" => NamedKey::Delete,
            "insert" | "ins" => NamedKey::Insert,
            "escape" | "esc" => NamedKey::Escape,
            "enter" | "return" => NamedKey::Enter,
            "space" => NamedKey::Space,
            "tab" => NamedKey::Tab,
            _ => return None,
        })
    }
}

/// The key half of a chord: one character (letters stored
/// upper-case), a function key (1–24), or a named key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    Char(char),
    Function(u8),
    Named(NamedKey),
}

impl Key {
    /// The canonical spelling in chord strings and menu labels.
    pub fn display(self) -> String {
        match self {
            Key::Char(c) => c.to_string(),
            Key::Function(n) => format!("F{n}"),
            Key::Named(named) => named.name().to_string(),
        }
    }
}

/// One keyboard chord: the modifier set plus the key. Two chords
/// are equal only when every modifier and the key match — that is
/// the comparison the shell's dispatch runs on every key press.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Chord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: Key,
}

impl Chord {
    pub fn new(key: Key) -> Self {
        Self {
            ctrl: false,
            alt: false,
            shift: false,
            key,
        }
    }

    /// The canonical chord string: modifiers in Ctrl, Alt, Shift
    /// order, then the key — `"Ctrl+Shift+F"`, `"Alt+Left"`, `"F3"`.
    /// Menu labels and the Guide print this form.
    pub fn display(&self) -> String {
        let mut out = String::new();
        if self.ctrl {
            out.push_str("Ctrl+");
        }
        if self.alt {
            out.push_str("Alt+");
        }
        if self.shift {
            out.push_str("Shift+");
        }
        out.push_str(&self.key.display());
        out
    }
}

/// Parse a chord string: modifiers (`Ctrl` / `Control`, `Alt`,
/// `Shift`, case-insensitive) joined by `+`, then the key — a
/// single character, an `F1`–`F24` function key, a named key
/// (`Left`, `PageUp`, `Enter`, …), or the words `Plus` / `Minus`
/// for the `+` / `-` keys. A trailing `++` also names the `+` key
/// (`"Ctrl++"`). Errors are plain words naming the bad part.
pub fn parse_chord(text: &str) -> Result<Chord, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("the chord is empty".to_string());
    }
    // Split off the key: a trailing "++" means the '+' key with the
    // modifiers before it; otherwise the last '+'-separated token
    // is the key.
    let (modifiers, key_token): (Vec<&str>, &str) = if trimmed == "+" {
        (Vec::new(), "+")
    } else if let Some(head) = trimmed.strip_suffix("++") {
        (
            head.split('+').filter(|part| !part.is_empty()).collect(),
            "+",
        )
    } else {
        let mut parts: Vec<&str> = trimmed.split('+').collect();
        let key = parts.pop().unwrap_or("");
        (parts, key)
    };
    let mut chord = Chord::new(Key::Char('\0'));
    for part in modifiers {
        match part.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" => chord.ctrl = true,
            "alt" => chord.alt = true,
            "shift" => chord.shift = true,
            other => return Err(format!("'{other}' is not a modifier (Ctrl, Alt or Shift)")),
        }
    }
    let token = key_token.trim();
    chord.key = parse_key_token(token)
        .ok_or_else(|| format!("'{token}' is not a key (a letter, F1-F24, or a named key)"))?;
    Ok(chord)
}

/// Parse the key half of a chord string (see [`parse_chord`]).
fn parse_key_token(token: &str) -> Option<Key> {
    if let Some(named) = NamedKey::parse(token) {
        return Some(Key::Named(named));
    }
    let lower = token.to_ascii_lowercase();
    if lower == "plus" {
        return Some(Key::Char('+'));
    }
    if lower == "minus" {
        return Some(Key::Char('-'));
    }
    if let Some(rest) = lower.strip_prefix('f') {
        if let Ok(n) = rest.parse::<u8>() {
            if (1..=24).contains(&n) && rest.chars().all(|c| c.is_ascii_digit()) {
                return Some(Key::Function(n));
            }
        }
    }
    let mut chars = token.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(Key::Char(c.to_ascii_uppercase())),
        _ => None,
    }
}

/// The rebindable actions and their default chords, in the order
/// the Guide lists them. `None` = the action exists (and can be
/// bound in the file) but has no default chord. This is the set
/// the shell's key dispatch is built from: the accelerator
/// commands of the menus, the bookmark keys, and the jump keys.
pub const ACTIONS: &[(&str, Option<&str>)] = &[
    ("open", Some("Ctrl+O")),
    ("save", Some("Ctrl+S")),
    ("save_as", Some("Ctrl+Shift+S")),
    ("undo", Some("Ctrl+Z")),
    ("redo", Some("Ctrl+Y")),
    ("cut", Some("Ctrl+X")),
    ("copy", Some("Ctrl+C")),
    ("paste", Some("Ctrl+V")),
    ("select_all", Some("Ctrl+A")),
    ("find", Some("Ctrl+F")),
    ("replace", Some("Ctrl+H")),
    ("find_next", Some("F3")),
    ("find_prev", Some("Shift+F3")),
    ("toggle_hex", Some("F6")),
    ("guide", Some("F1")),
    ("font_bigger", Some("Ctrl++")),
    ("font_smaller", Some("Ctrl+-")),
    ("toggle_bookmark", Some("Ctrl+F2")),
    ("bookmark_next", Some("F2")),
    ("bookmark_prev", Some("Shift+F2")),
    ("jump_back", Some("Alt+Left")),
    ("jump_forward", Some("Alt+Right")),
    ("pointer_follow", Some("Ctrl+Enter")),
    ("follow_tail", None),
    ("filter", None),
    ("clear_filter", None),
    // Tabs (roadmap item 39, slice R5b): the strip's commands.
    // Ctrl+1..Ctrl+8 jump to that tab; tab_last (Ctrl+9) jumps
    // to the final tab, however many there are.
    ("new_tab", Some("Ctrl+T")),
    ("close_tab", Some("Ctrl+W")),
    ("next_tab", Some("Ctrl+Tab")),
    ("prev_tab", Some("Ctrl+Shift+Tab")),
    ("tab_1", Some("Ctrl+1")),
    ("tab_2", Some("Ctrl+2")),
    ("tab_3", Some("Ctrl+3")),
    ("tab_4", Some("Ctrl+4")),
    ("tab_5", Some("Ctrl+5")),
    ("tab_6", Some("Ctrl+6")),
    ("tab_7", Some("Ctrl+7")),
    ("tab_8", Some("Ctrl+8")),
    ("tab_last", Some("Ctrl+9")),
    // Split view and side-by-side diff (roadmap item 28 and
    // the R5 diff, slice R5c). Ctrl+\ toggles the text/hex
    // split; F7 / Shift+F7 walk the compare's difference
    // regions.
    ("toggle_split", Some("Ctrl+\\")),
    ("diff_next", Some("F7")),
    ("diff_prev", Some("Shift+F7")),
];

/// Whether `name` is a known rebindable action.
pub fn is_action(name: &str) -> bool {
    ACTIONS.iter().any(|(action, _)| *action == name)
}

/// The merged keybindings: defaults overlaid with the owner's
/// file. Lookups run in [`ACTIONS`] order, so when two actions
/// share one chord the earlier action wins (and the collision is
/// reported as a problem at load).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyBindings {
    /// Action name → chord, for the bound actions only.
    map: BTreeMap<String, Chord>,
    /// Plain-words problems found while loading, in file order.
    problems: Vec<String>,
}

impl Default for KeyBindings {
    fn default() -> Self {
        Self::defaults()
    }
}

impl KeyBindings {
    /// The built-in defaults alone.
    pub fn defaults() -> Self {
        let mut bindings = Self {
            map: BTreeMap::new(),
            problems: Vec::new(),
        };
        for (action, chord_text) in ACTIONS {
            if let Some(text) = chord_text {
                let chord = parse_chord(text).expect("the built-in default chords must all parse");
                bindings.map.insert((*action).to_string(), chord);
            }
        }
        bindings
    }

    /// Parse `text` (the `keybindings.json` contents) over the
    /// defaults. Never fails outright: the result always holds a
    /// usable map, with every problem collected in
    /// [`KeyBindings::problems`].
    pub fn from_json(text: &str) -> Self {
        let mut bindings = Self::defaults();
        let pairs = match parse_flat_object(text) {
            Ok(pairs) => pairs,
            Err(message) => {
                bindings
                    .problems
                    .push(format!("keybindings.json: {message}"));
                return bindings;
            }
        };
        for (name, value) in pairs {
            if !is_action(&name) {
                bindings
                    .problems
                    .push(format!("keybindings.json: unknown action \"{name}\""));
                continue;
            }
            let value = value.trim();
            if value.is_empty() || value.eq_ignore_ascii_case("none") {
                bindings.map.remove(&name);
                continue;
            }
            match parse_chord(value) {
                Ok(chord) => {
                    bindings.map.insert(name, chord);
                }
                Err(message) => {
                    bindings.problems.push(format!(
                        "keybindings.json: \"{name}\": {message} (default kept)"
                    ));
                }
            }
        }
        // One chord driving two actions is a trap for the owner;
        // name it (the earlier action in ACTIONS order wins).
        let mut seen: Vec<(Chord, &str)> = Vec::new();
        for (action, _) in ACTIONS {
            let Some(chord) = bindings.map.get(*action) else {
                continue;
            };
            if let Some((_, first)) = seen.iter().find(|(c, _)| c == chord) {
                bindings.problems.push(format!(
                    "keybindings.json: {} is bound to both \"{first}\" and \"{action}\"; \"{first}\" wins",
                    chord.display()
                ));
            } else {
                seen.push((*chord, action));
            }
        }
        bindings
    }

    /// Load the bindings from `path`: a missing file means the
    /// defaults with no problems; an unreadable or malformed file
    /// means the defaults (or the readable parts) plus problems.
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_json(&text),
            Err(_) => Self::defaults(),
        }
    }

    /// The problems found while loading (empty when the file was
    /// missing or clean).
    pub fn problems(&self) -> &[String] {
        &self.problems
    }

    /// The chord bound to `action`, or `None` when the action is
    /// unbound (or unknown).
    pub fn chord_for(&self, action: &str) -> Option<Chord> {
        self.map.get(action).copied()
    }

    /// The chord bound to `action`, in display form — the menu
    /// label helper. `None` when unbound.
    pub fn label_for(&self, action: &str) -> Option<String> {
        self.chord_for(action).map(|chord| chord.display())
    }

    /// The action a chord triggers, or `None`. [`ACTIONS`] order
    /// decides collisions (see the type docs).
    pub fn action_for(&self, chord: &Chord) -> Option<&'static str> {
        ACTIONS
            .iter()
            .find(|(action, _)| self.map.get(*action) == Some(chord))
            .map(|(action, _)| *action)
    }
}

/// Parse the flat string→string object `keybindings.json` is:
/// `{ "name": "chord", ... }`. Tolerates whitespace and key
/// order; anything else (nested values, numbers, a non-object top
/// level) is an error naming the shape, and the caller falls back
/// to defaults. Hand-rolled in the house style of the settings and
/// bookmarks readers — deliberately not a general JSON parser.
fn parse_flat_object(text: &str) -> Result<Vec<(String, String)>, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut pos = 0usize;
    let peek = |pos: usize| chars.get(pos).copied();
    let skip_ws = |pos: &mut usize| {
        while matches!(peek(*pos), Some(c) if c.is_whitespace()) {
            *pos += 1;
        }
    };
    let read_string = |pos: &mut usize| -> Result<String, String> {
        skip_ws(pos);
        if peek(*pos) != Some('"') {
            return Err(format!("expected a string at character {}", *pos));
        }
        *pos += 1;
        let mut out = String::new();
        loop {
            match peek(*pos) {
                None => return Err("unterminated string".to_string()),
                Some('"') => {
                    *pos += 1;
                    return Ok(out);
                }
                Some('\\') => {
                    *pos += 1;
                    match peek(*pos) {
                        Some('"') => out.push('"'),
                        Some('\\') => out.push('\\'),
                        Some('/') => out.push('/'),
                        Some('n') => out.push('\n'),
                        Some('r') => out.push('\r'),
                        Some('t') => out.push('\t'),
                        _ => return Err("unknown escape in a string".to_string()),
                    }
                    *pos += 1;
                }
                Some(c) => {
                    out.push(c);
                    *pos += 1;
                }
            }
        }
    };
    skip_ws(&mut pos);
    if peek(pos) != Some('{') {
        return Err("the file is not a JSON object".to_string());
    }
    pos += 1;
    let mut pairs = Vec::new();
    loop {
        skip_ws(&mut pos);
        if peek(pos) == Some('}') {
            pos += 1;
            break;
        }
        let key = read_string(&mut pos)?;
        skip_ws(&mut pos);
        if peek(pos) != Some(':') {
            return Err(format!("expected ':' after \"{key}\""));
        }
        pos += 1;
        skip_ws(&mut pos);
        if peek(pos) != Some('"') {
            return Err(format!("the value for \"{key}\" is not a string"));
        }
        let value = read_string(&mut pos)?;
        pairs.push((key, value));
        skip_ws(&mut pos);
        if peek(pos) == Some(',') {
            pos += 1;
            continue;
        }
        if peek(pos) == Some('}') {
            pos += 1;
            break;
        }
        return Err("expected ',' or '}' in the object".to_string());
    }
    skip_ws(&mut pos);
    if pos != chars.len() {
        return Err("extra text follows the object".to_string());
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_the_accelerator_set() {
        let bindings = KeyBindings::defaults();
        assert!(bindings.problems().is_empty());
        assert_eq!(
            bindings.chord_for("find"),
            Some(Chord {
                ctrl: true,
                alt: false,
                shift: false,
                key: Key::Char('F'),
            })
        );
        assert_eq!(
            bindings.chord_for("toggle_hex"),
            Some(Chord::new(Key::Function(6)))
        );
        // Actions with no default chord are known but unbound.
        assert!(is_action("follow_tail"));
        assert_eq!(bindings.chord_for("follow_tail"), None);
        assert!(!is_action("explode"));
        // Every default resolves back to its own action.
        for (action, _) in ACTIONS {
            if let Some(chord) = bindings.chord_for(action) {
                assert_eq!(bindings.action_for(&chord), Some(*action), "{action}");
            }
        }
    }

    #[test]
    fn tab_actions_have_their_documented_defaults() {
        let bindings = KeyBindings::defaults();
        assert_eq!(bindings.label_for("new_tab").as_deref(), Some("Ctrl+T"));
        assert_eq!(bindings.label_for("close_tab").as_deref(), Some("Ctrl+W"));
        assert_eq!(bindings.label_for("next_tab").as_deref(), Some("Ctrl+Tab"));
        assert_eq!(
            bindings.label_for("prev_tab").as_deref(),
            Some("Ctrl+Shift+Tab")
        );
        assert_eq!(bindings.label_for("tab_1").as_deref(), Some("Ctrl+1"));
        assert_eq!(bindings.label_for("tab_8").as_deref(), Some("Ctrl+8"));
        assert_eq!(bindings.label_for("tab_last").as_deref(), Some("Ctrl+9"));
        assert_eq!(
            parse_chord("Ctrl+Tab").unwrap().key,
            Key::Named(NamedKey::Tab)
        );
    }

    #[test]
    fn chord_parsing_forms() {
        assert_eq!(
            parse_chord("Ctrl+Shift+F").unwrap(),
            Chord {
                ctrl: true,
                alt: false,
                shift: true,
                key: Key::Char('F'),
            }
        );
        // Case-insensitive, "Control" spelled out, loose spaces.
        assert_eq!(
            parse_chord(" control + s ").unwrap(),
            parse_chord("Ctrl+S").unwrap()
        );
        assert_eq!(
            parse_chord("alt+left").unwrap(),
            Chord {
                ctrl: false,
                alt: true,
                shift: false,
                key: Key::Named(NamedKey::Left),
            }
        );
        assert_eq!(parse_chord("F12").unwrap().key, Key::Function(12));
        assert_eq!(parse_chord("Shift+F3").unwrap().key, Key::Function(3));
        // The '+' key, both spellings; '-' likewise.
        assert_eq!(parse_chord("Ctrl++").unwrap().key, Key::Char('+'));
        assert_eq!(parse_chord("Ctrl+Plus").unwrap().key, Key::Char('+'));
        assert_eq!(parse_chord("+").unwrap(), Chord::new(Key::Char('+')));
        assert_eq!(parse_chord("Ctrl+-").unwrap().key, Key::Char('-'));
        assert_eq!(parse_chord("Ctrl+Minus").unwrap().key, Key::Char('-'));
        assert_eq!(
            parse_chord("Ctrl+Enter").unwrap().key,
            Key::Named(NamedKey::Enter)
        );
        // Bad chords are plain errors.
        assert!(parse_chord("").is_err());
        assert!(parse_chord("Ctrl+").is_err());
        assert!(parse_chord("Meta+F").is_err());
        assert!(parse_chord("Ctrl+F25").is_err());
        assert!(parse_chord("Ctrl+Foo").is_err());
    }

    #[test]
    fn chord_display_round_trips() {
        for text in [
            "Ctrl+O",
            "Ctrl+Shift+S",
            "F3",
            "Shift+F3",
            "Alt+Left",
            "Ctrl++",
            "Ctrl+-",
            "Ctrl+Enter",
            "F1",
        ] {
            assert_eq!(parse_chord(text).unwrap().display(), text);
        }
    }

    #[test]
    fn user_file_overrides_unbinds_and_reports() {
        let text = r#"{
            "find": "Ctrl+Shift+F",
            "filter": "Ctrl+Shift+L",
            "save": "",
            "bogus_action": "Ctrl+B",
            "undo": "Ctrl+Banana"
        }"#;
        let bindings = KeyBindings::from_json(text);
        // The override and the new binding land.
        assert_eq!(bindings.label_for("find").as_deref(), Some("Ctrl+Shift+F"));
        assert_eq!(
            bindings.label_for("filter").as_deref(),
            Some("Ctrl+Shift+L")
        );
        assert_eq!(
            bindings.action_for(&parse_chord("Ctrl+Shift+F").unwrap()),
            Some("find")
        );
        // The empty string unbinds.
        assert_eq!(bindings.chord_for("save"), None);
        // Untouched actions keep their defaults.
        assert_eq!(bindings.label_for("open").as_deref(), Some("Ctrl+O"));
        // Both problems are reported, in file order, and the bad
        // chord kept the default.
        assert_eq!(bindings.problems().len(), 2);
        assert!(bindings.problems()[0].contains("unknown action"));
        assert!(bindings.problems()[1].contains("\"undo\""));
        assert_eq!(bindings.label_for("undo").as_deref(), Some("Ctrl+Z"));
    }

    #[test]
    fn malformed_files_fall_back_to_defaults_with_a_problem() {
        for text in [
            "",
            "not json",
            "[1,2]",
            "{\"find\": 3}",
            "{\"find\": \"Ctrl+F\"",
        ] {
            let bindings = KeyBindings::from_json(text);
            assert_eq!(
                bindings.label_for("find").as_deref(),
                Some("Ctrl+F"),
                "{text}"
            );
            assert!(!bindings.problems().is_empty(), "{text}");
        }
        // An empty object is valid and problem-free.
        let bindings = KeyBindings::from_json("{}");
        assert!(bindings.problems().is_empty());
        assert_eq!(bindings, KeyBindings::defaults());
    }

    #[test]
    fn duplicate_chords_are_reported_and_the_first_action_wins() {
        let bindings = KeyBindings::from_json(r#"{"filter": "Ctrl+F"}"#);
        assert_eq!(bindings.problems().len(), 1);
        assert!(bindings.problems()[0].contains("both"));
        assert_eq!(
            bindings.action_for(&parse_chord("Ctrl+F").unwrap()),
            Some("find")
        );
    }

    #[test]
    fn missing_file_loads_clean_defaults() {
        let path = std::env::temp_dir().join("veloedit-no-such-keybindings-xyz.json");
        let bindings = KeyBindings::load_from(&path);
        assert_eq!(bindings, KeyBindings::defaults());
        assert!(bindings.problems().is_empty());
    }
}
