//! Built-in colour themes (owner request 2026-10-31: "a lot of themes,
//! similar to Notepad++ themes").
//!
//! Every theme is a complete token set for the editor surface: canvas
//! background, panels, primary text, muted metadata, boundary lines, the
//! line-number gutter, the current-line wash, the selection tint, the
//! search-match tint, and the accent (caret, selection derivation,
//! current-line number, active mode tab, scrollbar thumb).
//!
//! The palettes are VeloEdit's own token values written in the style of
//! the named classic editor themes — no theme file from any other
//! program is copied or loaded. There are exactly 18 built-ins; the first
//! is **Obsidian**, the SPEC.md section 4.1 palette verbatim, and it is
//! the default.
//!
//! The owner's accent picker sits on top of any theme: an accent
//! override replaces the theme's accent and the one token derived from
//! it at build time (the selection tint) — see
//! [`Theme::with_accent_override`]. Passing no override hands the accent
//! back to the theme ("theme default").
//!
//! VeloEdit has no syntax lexer in v1, so themes colour the chrome and
//! the text uniformly; there are no per-language colours to define.

/// One colour: red, green, blue, alpha (255 = opaque). Tokens that act
/// as washes over the canvas (current-line, selection, match) carry
/// their alpha here; the renderer composites them with a translucent
/// Direct2D brush, in that order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    /// An opaque colour from red/green/blue components.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// The same colour at a different opacity.
    pub const fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }

    /// Parse `#RRGGBB`, `RRGGBB`, `#RRGGBBAA`, or `RRGGBBAA`
    /// (case-insensitive). Anything else is `None` — a bad hex string is
    /// a caller error to report, never a colour to guess.
    pub fn from_hex(text: &str) -> Option<Self> {
        let digits = text.strip_prefix('#').unwrap_or(text);
        if digits.len() != 6 && digits.len() != 8 {
            return None;
        }
        if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let component = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).ok();
        Some(Self {
            r: component(0)?,
            g: component(2)?,
            b: component(4)?,
            a: if digits.len() == 8 {
                component(6)?
            } else {
                255
            },
        })
    }

    /// Format as `#RRGGBB`, or `#RRGGBBAA` when the colour is not opaque.
    /// Uppercase digits, so a format → parse round trip is exact.
    pub fn to_hex(&self) -> String {
        if self.a == 255 {
            format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
        } else {
            format!("#{:02X}{:02X}{:02X}{:02X}", self.r, self.g, self.b, self.a)
        }
    }

    /// This colour blended over an opaque `base`, in plain sRGB space —
    /// the effective colour a translucent token shows on the canvas.
    /// Used by the contrast tests; the renderer itself lets Direct2D
    /// composite.
    pub fn over(&self, base: Rgba) -> Rgba {
        let t = self.a as f32 / 255.0;
        let mix = |fg: u8, bg: u8| (fg as f32 * t + bg as f32 * (1.0 - t)).round() as u8;
        Rgba::rgb(
            mix(self.r, base.r),
            mix(self.g, base.g),
            mix(self.b, base.b),
        )
    }

    /// WCAG relative luminance of the (opaque) colour.
    fn luminance(&self) -> f32 {
        let channel = |c: u8| {
            let s = c as f32 / 255.0;
            if s <= 0.03928 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(self.r) + 0.7152 * channel(self.g) + 0.0722 * channel(self.b)
    }

    /// WCAG contrast ratio between two opaque colours, in [1, 21].
    pub fn contrast_ratio(&self, other: Rgba) -> f32 {
        let (hi, lo) = {
            let (a, b) = (self.luminance(), other.luminance());
            if a >= b {
                (a, b)
            } else {
                (b, a)
            }
        };
        (hi + 0.05) / (lo + 0.05)
    }
}

/// A complete editor colour scheme. All fields are [`Rgba`]; washes
/// (current-line, selection, match) carry alpha and are composited over
/// the canvas in draw order: current-line first, then selection, then
/// match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    pub name: &'static str,
    /// Editor canvas background.
    pub bg: Rgba,
    /// Menu-adjacent strips, find bar, status bar, dialog panels.
    pub panel: Rgba,
    /// Primary text.
    pub text: Rgba,
    /// Line numbers, hex offsets, status metadata.
    pub muted: Rgba,
    /// Boundary lines and splitters.
    pub line: Rgba,
    /// Line-number gutter background.
    pub gutter_bg: Rgba,
    /// Wash over the row holding the caret.
    pub current_line: Rgba,
    /// Tint over selected text (derived from the accent).
    pub selection: Rgba,
    /// Tint over the current search match.
    pub match_bg: Rgba,
    /// Caret, current-line number, active tab, scrollbar thumb. The one
    /// token the owner's accent picker can override.
    pub accent: Rgba,
}

/// Alpha of the accent wash used for the selection tint.
const SELECTION_ALPHA: u8 = 0x59; // ≈35%

/// The theme constructor used by the table below: the wash tokens are
/// derived, not stored per theme — the current-line wash is the theme's
/// text colour at low alpha (a light wash on dark themes, a dark wash on
/// light ones, always in the direction that stays visible), the
/// selection tint is the accent at [`SELECTION_ALPHA`], and the gutter
/// sits on the panel colour.
// One row of the built-in palette table: the eight values are the
// theme's identity, so they stay as separate parameters on purpose.
#[allow(clippy::too_many_arguments)]
const fn build(
    name: &'static str,
    bg: Rgba,
    panel: Rgba,
    text: Rgba,
    muted: Rgba,
    line: Rgba,
    accent: Rgba,
    match_colour: Rgba,
) -> Theme {
    Theme {
        name,
        bg,
        panel,
        text,
        muted,
        line,
        gutter_bg: panel,
        current_line: text.with_alpha(0x14), // ≈8%
        selection: accent.with_alpha(SELECTION_ALPHA),
        match_bg: match_colour.with_alpha(0x66), // ≈40%
        accent,
    }
}

/// Shorthand so the table below stays readable.
const fn c(r: u8, g: u8, b: u8) -> Rgba {
    Rgba::rgb(r, g, b)
}

/// The default theme name (SPEC.md section 4.1).
pub const DEFAULT_THEME_NAME: &str = "Obsidian";

/// All 18 built-in themes, in menu order. Obsidian (the default) first,
/// then the one light theme from the owner's theme spec, then the
/// classics in the order the owner named them.
pub fn builtins() -> Vec<Theme> {
    vec![
        // Obsidian — SPEC.md section 4.1 verbatim. Selection in the
        // approved wireframe is the accent at low alpha over the canvas.
        build(
            "Obsidian",
            c(0x0E, 0x0E, 0x10),
            c(0x16, 0x16, 0x1A),
            c(0xE6, 0xE6, 0xE6),
            c(0x7A, 0x7A, 0x85),
            c(0x2A, 0x2A, 0x30),
            c(0xE0, 0x24, 0x24),
            c(0xB5, 0x89, 0x00),
        ),
        // Light — the owner's light option: #F4F4F6 family.
        build(
            "Light",
            c(0xF4, 0xF4, 0xF6),
            c(0xFF, 0xFF, 0xFF),
            c(0x1B, 0x1B, 0x1F),
            c(0x6A, 0x6A, 0x75),
            c(0xD4, 0xD4, 0xDC),
            c(0xC8, 0x1E, 0x1E),
            c(0xE0, 0xA8, 0x00),
        ),
        build(
            "Solarized Dark",
            c(0x00, 0x2B, 0x36),
            c(0x07, 0x36, 0x42),
            c(0x93, 0xA1, 0xA1),
            c(0x65, 0x7B, 0x83),
            c(0x08, 0x45, 0x54),
            c(0x26, 0x8B, 0xD2),
            c(0xB5, 0x89, 0x00),
        ),
        build(
            "Solarized Light",
            c(0xFD, 0xF6, 0xE3),
            c(0xEE, 0xE8, 0xD5),
            c(0x58, 0x6E, 0x75),
            c(0x93, 0xA1, 0xA1),
            c(0xD8, 0xCF, 0xB8),
            c(0x26, 0x8B, 0xD2),
            c(0xB5, 0x89, 0x00),
        ),
        build(
            "Monokai",
            c(0x27, 0x28, 0x22),
            c(0x1E, 0x1F, 0x1A),
            c(0xF8, 0xF8, 0xF2),
            c(0x75, 0x71, 0x5E),
            c(0x3E, 0x3D, 0x32),
            c(0xF9, 0x26, 0x72),
            c(0xE6, 0xDB, 0x74),
        ),
        build(
            "Dracula",
            c(0x28, 0x2A, 0x36),
            c(0x21, 0x22, 0x2C),
            c(0xF8, 0xF8, 0xF2),
            c(0x62, 0x72, 0xA4),
            c(0x44, 0x47, 0x5A),
            c(0xFF, 0x79, 0xC6),
            c(0xF1, 0xFA, 0x8C),
        ),
        build(
            "Zenburn",
            c(0x3F, 0x3F, 0x3F),
            c(0x35, 0x35, 0x35),
            c(0xDC, 0xDC, 0xCC),
            c(0x7F, 0x9F, 0x7F),
            c(0x55, 0x57, 0x53),
            c(0xCC, 0x93, 0x93),
            c(0xF0, 0xDF, 0xAF),
        ),
        build(
            "Twilight",
            c(0x14, 0x14, 0x14),
            c(0x1E, 0x1E, 0x1E),
            c(0xDA, 0xDA, 0xDA),
            c(0x8A, 0x8A, 0x8A),
            c(0x30, 0x30, 0x30),
            c(0xCD, 0xA8, 0x69),
            c(0xE9, 0xC9, 0x79),
        ),
        build(
            "Black Board",
            c(0x0C, 0x10, 0x21),
            c(0x11, 0x16, 0x2B),
            c(0xF8, 0xF8, 0xF8),
            c(0x6A, 0x73, 0x94),
            c(0x25, 0x31, 0x58),
            c(0xFB, 0xDE, 0x2D),
            c(0xFF, 0x64, 0x00),
        ),
        build(
            "Choco",
            c(0x2B, 0x1D, 0x14),
            c(0x36, 0x26, 0x1B),
            c(0xEF, 0xDD, 0xBB),
            c(0xA6, 0x8B, 0x64),
            c(0x4A, 0x32, 0x20),
            c(0xFF, 0x9D, 0x45),
            c(0xD9, 0xA4, 0x41),
        ),
        build(
            "Deep Black",
            c(0x00, 0x00, 0x00),
            c(0x0A, 0x0A, 0x0A),
            c(0xD4, 0xD4, 0xD4),
            c(0x80, 0x80, 0x80),
            c(0x26, 0x26, 0x26),
            c(0xFF, 0xCC, 0x00),
            c(0x29, 0x79, 0xFF),
        ),
        build(
            "Mossy Lawn",
            c(0x20, 0x30, 0x1F),
            c(0x2A, 0x3D, 0x28),
            c(0xD8, 0xE6, 0xC3),
            c(0x8F, 0xA3, 0x82),
            c(0x3E, 0x57, 0x41),
            c(0xA4, 0xC6, 0x39),
            c(0xE8, 0xD4, 0x4D),
        ),
        build(
            "Navajo",
            c(0xFF, 0xEE, 0xCF),
            c(0xFF, 0xE8, 0xBC),
            c(0x4A, 0x32, 0x20),
            c(0xA0, 0x84, 0x64),
            c(0xE3, 0xC1, 0x88),
            c(0xC1, 0x44, 0x0E),
            c(0xF2, 0xB3, 0x3D),
        ),
        build(
            "Ruby Blue",
            c(0x0E, 0x22, 0x33),
            c(0x16, 0x28, 0x3D),
            c(0xBF, 0xD9, 0xE8),
            c(0x5F, 0x7A, 0x8C),
            c(0x24, 0x42, 0x5C),
            c(0xE0, 0x11, 0x5F),
            c(0xF6, 0xC1, 0x77),
        ),
        build(
            "Bespin",
            c(0x28, 0x21, 0x1C),
            c(0x1F, 0x1A, 0x16),
            c(0xD8, 0xC0, 0x9A),
            c(0x8A, 0x71, 0x5C),
            c(0x3A, 0x2E, 0x26),
            c(0xCF, 0x6A, 0x4C),
            c(0xF9, 0xEE, 0x98),
        ),
        build(
            "Hot Fudge Sundae",
            c(0x24, 0x16, 0x11),
            c(0x32, 0x20, 0x16),
            c(0xF3, 0xE3, 0xCE),
            c(0xB0, 0x8D, 0x6D),
            c(0x47, 0x2C, 0x1E),
            c(0xFF, 0x52, 0x52),
            c(0xFF, 0xC4, 0x6B),
        ),
        build(
            "Plastic Code Wrap",
            c(0x0D, 0x1B, 0x2E),
            c(0x13, 0x2A, 0x44),
            c(0xD6, 0xE4, 0xF0),
            c(0x6E, 0x8C, 0xA0),
            c(0x27, 0x4B, 0x73),
            c(0x4F, 0xC3, 0xF7),
            c(0xFF, 0xE0, 0x82),
        ),
        build(
            "Vim Dark Blue",
            c(0x00, 0x00, 0x28),
            c(0x0A, 0x0A, 0x3C),
            c(0xF8, 0xF8, 0xF8),
            c(0x7A, 0x86, 0xC8),
            c(0x2A, 0x2A, 0x6E),
            c(0xFF, 0xCF, 0x00),
            c(0x00, 0xAF, 0xAF),
        ),
    ]
}

/// Names of all built-in themes, in menu order.
pub fn names() -> Vec<&'static str> {
    builtins().iter().map(|t| t.name).collect()
}

/// Look a theme up by name (exact, then case-insensitive). An unknown
/// name is `None` — callers fall back to the default, they never guess.
pub fn by_name(name: &str) -> Option<Theme> {
    let all = builtins();
    all.iter()
        .find(|t| t.name == name)
        .or_else(|| all.iter().find(|t| t.name.eq_ignore_ascii_case(name)))
        .cloned()
}

/// The default theme (Obsidian, SPEC.md section 4.1).
pub fn default_theme() -> Theme {
    by_name(DEFAULT_THEME_NAME).expect("the default theme is in the built-in table")
}

impl Theme {
    /// This theme with the accent replaced by the owner's chosen colour
    /// (a hex string, see [`Rgba::from_hex`]). Only the accent-derived
    /// tokens change: the accent itself and the selection tint derived
    /// from it. Everything else — including the match tint — keeps the
    /// theme's own values. An unparseable colour returns the theme
    /// unchanged; callers validate the hex before storing it.
    pub fn with_accent_override(&self, hex: &str) -> Theme {
        match Rgba::from_hex(hex) {
            Some(colour) => {
                let mut out = self.clone();
                out.accent = colour.rgb_opaque();
                out.selection = out.accent.with_alpha(SELECTION_ALPHA);
                out
            }
            None => self.clone(),
        }
    }

    /// Resolve a stored theme name plus optional accent override into
    /// the theme the renderer should use. Unknown theme names fall back
    /// to the default theme; the override still applies on top of it.
    pub fn resolve(name: &str, accent_override: Option<&str>) -> Theme {
        let base = by_name(name).unwrap_or_else(default_theme);
        match accent_override {
            Some(hex) => base.with_accent_override(hex),
            None => base,
        }
    }
}

impl Rgba {
    /// The colour with its alpha forced opaque (accent colours are
    /// stored and compared as opaque RGB).
    const fn rgb_opaque(self) -> Rgba {
        Rgba { a: 255, ..self }
    }
}

/// Preset accent swatches offered next to "Theme default" and the
/// custom colour picker: (display name, hex). Crimson is the SPEC.md
/// section 4.1 accent and the default.
pub const ACCENT_PRESETS: &[(&str, &str)] = &[
    ("Crimson", "#E02424"),
    ("Blue", "#2F7CF6"),
    ("Teal", "#00AFAF"),
    ("Green", "#43A047"),
    ("Orange", "#F07D1A"),
    ("Purple", "#8E44AD"),
    ("Pink", "#E75480"),
    ("Gold", "#C9A227"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_18_builtins_with_unique_names() {
        let all = builtins();
        assert_eq!(all.len(), 18);
        let mut seen: Vec<&str> = all.iter().map(|t| t.name).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 18, "theme names must be unique");
        assert_eq!(all[0].name, "Obsidian");
        assert_eq!(names().len(), 18);
    }

    #[test]
    fn obsidian_is_the_spec_palette_verbatim() {
        // SPEC.md section 4.1, token for token.
        let t = default_theme();
        assert_eq!(t.name, "Obsidian");
        assert_eq!(t.bg, Rgba::from_hex("#0E0E10").unwrap());
        assert_eq!(t.panel, Rgba::from_hex("#16161A").unwrap());
        assert_eq!(t.accent, Rgba::from_hex("#E02424").unwrap());
        assert_eq!(t.text, Rgba::from_hex("#E6E6E6").unwrap());
        assert_eq!(t.muted, Rgba::from_hex("#7A7A85").unwrap());
        assert_eq!(t.line, Rgba::from_hex("#2A2A30").unwrap());
    }

    #[test]
    fn light_theme_is_the_owner_spec_family() {
        let t = by_name("Light").unwrap();
        assert_eq!(t.bg, Rgba::from_hex("#F4F4F6").unwrap());
        assert_eq!(t.panel, Rgba::from_hex("#FFFFFF").unwrap());
        assert_eq!(t.text, Rgba::from_hex("#1B1B1F").unwrap());
    }

    #[test]
    fn every_token_of_every_theme_round_trips_through_hex() {
        for t in builtins() {
            for colour in [
                t.bg,
                t.panel,
                t.text,
                t.muted,
                t.line,
                t.gutter_bg,
                t.current_line,
                t.selection,
                t.match_bg,
                t.accent,
            ] {
                assert_eq!(
                    Rgba::from_hex(&colour.to_hex()),
                    Some(colour),
                    "{} token {:?}",
                    t.name,
                    colour
                );
            }
        }
    }

    #[test]
    fn text_on_background_contrast_is_at_least_aa_for_every_theme() {
        for t in builtins() {
            let ratio = t.text.contrast_ratio(t.bg);
            assert!(
                ratio >= 4.5,
                "{}: text {:?} on bg {:?} has contrast {ratio:.2} < 4.5",
                t.name,
                t.text,
                t.bg
            );
        }
    }

    #[test]
    fn derived_tokens_have_the_documented_relationships() {
        for t in builtins() {
            assert_eq!(t.gutter_bg, t.panel, "{}", t.name);
            assert_eq!(t.current_line.a, 0x14, "{}", t.name);
            assert_eq!(
                t.current_line.rgb_opaque(),
                t.text.rgb_opaque(),
                "{}",
                t.name
            );
            assert_eq!(t.selection.a, SELECTION_ALPHA, "{}", t.name);
            assert_eq!(t.selection.rgb_opaque(), t.accent, "{}", t.name);
            assert_eq!(t.accent.a, 255, "{}", t.name);
            assert_eq!(t.bg.a, 255, "{}", t.name);
            assert_eq!(t.text.a, 255, "{}", t.name);
        }
    }

    #[test]
    fn accent_override_replaces_only_accent_derived_tokens() {
        let base = default_theme();
        let over = base.with_accent_override("#2F7CF6");
        assert_eq!(over.accent, Rgba::from_hex("#2F7CF6").unwrap());
        assert_eq!(over.selection.rgb_opaque(), over.accent);
        assert_eq!(over.selection.a, base.selection.a);
        // Everything not derived from the accent is untouched.
        assert_eq!(over.bg, base.bg);
        assert_eq!(over.panel, base.panel);
        assert_eq!(over.text, base.text);
        assert_eq!(over.muted, base.muted);
        assert_eq!(over.line, base.line);
        assert_eq!(over.gutter_bg, base.gutter_bg);
        assert_eq!(over.current_line, base.current_line);
        assert_eq!(over.match_bg, base.match_bg);
        assert_eq!(over.name, base.name);
        // Case and the leading '#' are both flexible.
        assert_eq!(base.with_accent_override("2f7cf6").accent, over.accent);
        // A bad hex string changes nothing.
        assert_eq!(base.with_accent_override("#XYZXYZ"), base);
        assert_eq!(base.with_accent_override(""), base);
    }

    #[test]
    fn resolve_falls_back_and_applies_overrides() {
        assert_eq!(Theme::resolve("No Such Theme", None), default_theme());
        assert_eq!(Theme::resolve("monokai", None).name, "Monokai");
        let t = Theme::resolve("Monokai", Some("#00AFAF"));
        assert_eq!(t.name, "Monokai");
        assert_eq!(t.accent, Rgba::from_hex("#00AFAF").unwrap());
    }

    #[test]
    fn hex_parsing_edges() {
        assert_eq!(
            Rgba::from_hex("#E02424"),
            Some(Rgba {
                r: 0xE0,
                g: 0x24,
                b: 0x24,
                a: 255
            })
        );
        assert_eq!(
            Rgba::from_hex("e0242480"),
            Some(Rgba {
                r: 0xE0,
                g: 0x24,
                b: 0x24,
                a: 0x80
            })
        );
        assert_eq!(Rgba::from_hex("#12345"), None);
        assert_eq!(Rgba::from_hex("#1234567"), None);
        assert_eq!(Rgba::from_hex("#GGGGGG"), None);
        assert_eq!(Rgba::from_hex(""), None);
        assert_eq!(
            Rgba {
                r: 1,
                g: 2,
                b: 3,
                a: 255
            }
            .to_hex(),
            "#010203"
        );
        assert_eq!(
            Rgba {
                r: 1,
                g: 2,
                b: 3,
                a: 4
            }
            .to_hex(),
            "#01020304"
        );
    }

    #[test]
    fn accent_presets_are_valid_and_include_crimson() {
        assert_eq!(ACCENT_PRESETS.len(), 8);
        for (_, hex) in ACCENT_PRESETS {
            assert!(Rgba::from_hex(hex).is_some(), "{hex}");
        }
        assert_eq!(ACCENT_PRESETS[0], ("Crimson", "#E02424"));
    }
}
