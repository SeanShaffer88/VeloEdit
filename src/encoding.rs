//! Text encoding detection, decoding, and encoding (owner-confirmed v1
//! feature, 2026-10-31 — the blueprint covers bytes and hex only, so this
//! module is the whole encoding story).
//!
//! The piece table always stores bytes. Encodings matter at the edges:
//! detecting what a file's bytes mean when it is opened, decoding byte
//! ranges to text for display and for building search needles, and
//! encoding edited text back to bytes on save.
//!
//! # Detection rules ([`detect`])
//!
//! Applied in this order to a sample (the first chunk of a file):
//!
//! 1. **Byte-order marks win.** `EF BB BF` → UTF-8, `FF FE` → UTF-16 LE,
//!    `FE FF` → UTF-16 BE. The BOM length is reported so callers can skip
//!    it, and the verdict is confident.
//! 2. **Empty sample** → UTF-8, confident (an empty file has no encoding
//!    evidence; UTF-8 is the program default).
//! 3. **NUL-parity pattern** for BOM-less UTF-16: in the first
//!    up-to-512 bytes, if NUL bytes appear at offsets of one parity only
//!    (odd offsets → LE, even offsets → BE), number at least two, and make
//!    up at least a quarter of the sampled bytes, while the sample length
//!    is even, the sample is that flavour of UTF-16. This check runs
//!    *before* the UTF-8 check even though it is conceptually the second
//!    heuristic: BOM-less UTF-16 holding plain ASCII text is itself valid
//!    UTF-8 byte-wise (every second byte is `0x00`, which UTF-8 allows),
//!    so accepting UTF-8 first would make BOM-less UTF-16 undetectable.
//!    A real UTF-8 text file essentially never has a quarter of its bytes
//!    as NULs in a single parity, so the reordering is safe.
//! 4. **Strict UTF-8**: if the sample validates as UTF-8 the verdict is
//!    UTF-8, confident. A sample cut from a larger file may end in the
//!    middle of one multi-byte character; a truncated final sequence
//!    alone (the validator reports "incomplete" at the tail, with every
//!    preceding byte valid) does not flip the verdict.
//! 5. **Otherwise ANSI** (Windows-1252), marked *not* confident: the
//!    bytes are probably legacy 8-bit text, but they could also be a
//!    binary file or a less common encoding this program does not claim.
//!
//! # Decode / encode strictness
//!
//! Decoding never silently substitutes replacement characters. UTF-16
//! input with an odd trailing byte or a lone surrogate is a
//! [`DecodeError`] naming the byte position. ANSI decoding is total (every
//! byte maps to exactly one character) and round-trips losslessly — see
//! the note on the undefined Windows-1252 bytes in
//! [`decode_byte_ansi`]. Encoding text into ANSI fails with an
//! [`EncodeError`] for any character Windows-1252 cannot represent;
//! nothing is ever replaced with `'?'` behind the caller's back.
//!
//! [`decode`] does not strip a byte-order mark: a decoded BOM appears as
//! `U+FEFF` at the start of the string. Callers opening a file skip
//! `Detection::bom_len` bytes first. [`encode`] never writes a BOM;
//! [`bom_bytes`] returns one when a caller wants to write it explicitly.

use std::fmt;

/// A text encoding VeloEdit can detect, decode, and encode.
///
/// `Ansi` means Windows-1252, the legacy 8-bit encoding of Western
/// European Windows installs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Ansi,
}

impl TextEncoding {
    /// Short lower-case name, as used by the `--encoding` CLI flag.
    pub fn name(&self) -> &'static str {
        match self {
            TextEncoding::Utf8 => "utf8",
            TextEncoding::Utf16Le => "utf16le",
            TextEncoding::Utf16Be => "utf16be",
            TextEncoding::Ansi => "ansi",
        }
    }
}

/// The result of [`detect`]: what the sample most likely is, how many
/// leading bytes are a byte-order mark, and whether the verdict is
/// certain (BOM or strict UTF-8) or a best guess (ANSI fallback, and the
/// NUL-parity heuristic is reported confident only for a strong pattern).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Detection {
    pub encoding: TextEncoding,
    /// Length of the byte-order mark at the start of the sample (0, 2, or 3).
    pub bom_len: usize,
    pub confident: bool,
}

/// Decoding failed. `position` is the byte offset in the input where the
/// problem starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError {
    pub position: usize,
    pub message: String,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "decode error at byte {}: {}",
            self.position, self.message
        )
    }
}

impl std::error::Error for DecodeError {}

/// Encoding failed. `position` is the byte offset in the input string
/// where the unrepresentable character starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodeError {
    pub position: usize,
    pub ch: char,
    pub message: String,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "encode error at character byte {}: {}",
            self.position, self.message
        )
    }
}

impl std::error::Error for EncodeError {}

/// The byte-order mark for an encoding, for callers that write one
/// explicitly. ANSI has no BOM.
pub fn bom_bytes(enc: TextEncoding) -> &'static [u8] {
    match enc {
        TextEncoding::Utf8 => &[0xEF, 0xBB, 0xBF],
        TextEncoding::Utf16Le => &[0xFF, 0xFE],
        TextEncoding::Utf16Be => &[0xFE, 0xFF],
        TextEncoding::Ansi => &[],
    }
}

/// Detect the encoding of a byte sample. The rules, in order, are
/// documented at the top of this module.
pub fn detect(sample: &[u8]) -> Detection {
    // Rule 1: byte-order marks.
    if sample.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Detection {
            encoding: TextEncoding::Utf8,
            bom_len: 3,
            confident: true,
        };
    }
    if sample.starts_with(&[0xFF, 0xFE]) {
        return Detection {
            encoding: TextEncoding::Utf16Le,
            bom_len: 2,
            confident: true,
        };
    }
    if sample.starts_with(&[0xFE, 0xFF]) {
        return Detection {
            encoding: TextEncoding::Utf16Be,
            bom_len: 2,
            confident: true,
        };
    }
    // Rule 2: empty sample.
    if sample.is_empty() {
        return Detection {
            encoding: TextEncoding::Utf8,
            bom_len: 0,
            confident: true,
        };
    }
    // Rule 3: NUL-parity pattern (BOM-less UTF-16). Runs before the UTF-8
    // acceptance for the reason given in the module docs.
    if sample.len() % 2 == 0 {
        let window_len = sample.len().min(512) & !1;
        let window = &sample[..window_len];
        let nul_even = window.iter().step_by(2).filter(|&&b| b == 0).count();
        let nul_odd = window
            .iter()
            .skip(1)
            .step_by(2)
            .filter(|&&b| b == 0)
            .count();
        if nul_odd >= 2 && nul_even == 0 && nul_odd * 4 >= window_len {
            return Detection {
                encoding: TextEncoding::Utf16Le,
                bom_len: 0,
                confident: true,
            };
        }
        if nul_even >= 2 && nul_odd == 0 && nul_even * 4 >= window_len {
            return Detection {
                encoding: TextEncoding::Utf16Be,
                bom_len: 0,
                confident: true,
            };
        }
    }
    // Rule 4: strict UTF-8 (a truncated final sequence still counts).
    if is_valid_utf8_sample(sample) {
        return Detection {
            encoding: TextEncoding::Utf8,
            bom_len: 0,
            confident: true,
        };
    }
    // Rule 5: legacy 8-bit fallback, never confident.
    Detection {
        encoding: TextEncoding::Ansi,
        bom_len: 0,
        confident: false,
    }
}

/// True when `sample` is valid UTF-8, allowing the final character to be
/// cut off mid-sequence (a sample taken from the start of a larger file):
/// `std` reports that case as an error with `error_len() == None` and all
/// bytes before `valid_up_to()` valid, which is exactly the cut-off case.
fn is_valid_utf8_sample(sample: &[u8]) -> bool {
    match std::str::from_utf8(sample) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none(),
    }
}

/// Decode bytes in encoding `enc` to a string. Strict: malformed input is
/// a [`DecodeError`] with the byte position, never a silent replacement.
pub fn decode(bytes: &[u8], enc: TextEncoding) -> Result<String, DecodeError> {
    match enc {
        TextEncoding::Utf8 => std::str::from_utf8(bytes)
            .map(|s| s.to_string())
            .map_err(|e| DecodeError {
                position: e.valid_up_to(),
                message: "invalid UTF-8".to_string(),
            }),
        TextEncoding::Utf16Le => decode_utf16(bytes, true),
        TextEncoding::Utf16Be => decode_utf16(bytes, false),
        TextEncoding::Ansi => Ok(bytes.iter().map(|&b| decode_byte_ansi(b)).collect()),
    }
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> Result<String, DecodeError> {
    let unit = |i: usize| -> u16 {
        if little_endian {
            u16::from_le_bytes([bytes[i], bytes[i + 1]])
        } else {
            u16::from_be_bytes([bytes[i], bytes[i + 1]])
        }
    };
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 >= bytes.len() {
            return Err(DecodeError {
                position: i,
                message: "odd trailing byte: UTF-16 input has an incomplete final code unit"
                    .to_string(),
            });
        }
        let u = unit(i);
        match u {
            0xD800..=0xDBFF => {
                // High surrogate: a low surrogate must follow.
                if i + 3 >= bytes.len() {
                    return Err(DecodeError {
                        position: i,
                        message: "lone high surrogate at end of UTF-16 input".to_string(),
                    });
                }
                let lo = unit(i + 2);
                if !(0xDC00..=0xDFFF).contains(&lo) {
                    return Err(DecodeError {
                        position: i,
                        message: "high surrogate not followed by a low surrogate".to_string(),
                    });
                }
                let cp = 0x1_0000u32 + (((u as u32) - 0xD800) << 10) + ((lo as u32) - 0xDC00);
                out.push(
                    char::from_u32(cp).expect("a checked surrogate pair is a valid code point"),
                );
                i += 4;
            }
            0xDC00..=0xDFFF => {
                return Err(DecodeError {
                    position: i,
                    message: "lone low surrogate in UTF-16 input".to_string(),
                });
            }
            _ => {
                out.push(
                    char::from_u32(u as u32)
                        .expect("a non-surrogate BMP code unit is a valid code point"),
                );
                i += 2;
            }
        }
    }
    Ok(out)
}

/// The Windows-1252 character for one byte.
///
/// Bytes `0x00..=0x7F` are ASCII identity and `0xA0..=0xFF` are the
/// Latin-1 code points of the same value. The `0x80..=0x9F` range uses the
/// real Windows-1252 punctuation table — except the five bytes Microsoft
/// leaves undefined (`0x81`, `0x8D`, `0x8F`, `0x90`, `0x9D`), which decode
/// to the matching C1 control character (`U+0081` etc.). That choice
/// makes decode → encode an exact round trip for all 256 byte values:
/// no byte is ever lost or rewritten by a decode/encode cycle.
pub fn decode_byte_ansi(byte: u8) -> char {
    match byte {
        0x00..=0x7F | 0xA0..=0xFF => byte as char,
        0x80..=0x9F => CP1252_C1[(byte - 0x80) as usize],
    }
}

/// Windows-1252 assignments for bytes `0x80..=0x9F`. The undefined slots
/// hold the C1 control characters of the same value (see
/// [`decode_byte_ansi`]).
const CP1252_C1: [char; 32] = [
    '\u{20AC}', // 0x80 €
    '\u{0081}', // 0x81 (undefined → C1 control)
    '\u{201A}', // 0x82 ‚
    '\u{0192}', // 0x83 ƒ
    '\u{201E}', // 0x84 „
    '\u{2026}', // 0x85 …
    '\u{2020}', // 0x86 †
    '\u{2021}', // 0x87 ‡
    '\u{02C6}', // 0x88 ˆ
    '\u{2030}', // 0x89 ‰
    '\u{0160}', // 0x8A Š
    '\u{2039}', // 0x8B ‹
    '\u{0152}', // 0x8C Œ
    '\u{008D}', // 0x8D (undefined → C1 control)
    '\u{017D}', // 0x8E Ž
    '\u{008F}', // 0x8F (undefined → C1 control)
    '\u{0090}', // 0x90 (undefined → C1 control)
    '\u{2018}', // 0x91 ‘
    '\u{2019}', // 0x92 ’
    '\u{201C}', // 0x93 “
    '\u{201D}', // 0x94 ”
    '\u{2022}', // 0x95 •
    '\u{2013}', // 0x96 –
    '\u{2014}', // 0x97 —
    '\u{02DC}', // 0x98 ˜
    '\u{2122}', // 0x99 ™
    '\u{0161}', // 0x9A š
    '\u{203A}', // 0x9B ›
    '\u{0153}', // 0x9C œ
    '\u{009D}', // 0x9D (undefined → C1 control)
    '\u{017E}', // 0x9E ž
    '\u{0178}', // 0x9F Ÿ
];

/// The Windows-1252 byte for a character, or `None` when Windows-1252
/// cannot represent it. Exact inverse of [`decode_byte_ansi`]: note this
/// means the C1 controls `U+0080..=U+009F` are *not* representable except
/// for the five values the undefined bytes decode to, because those byte
/// positions carry punctuation instead.
fn encode_char_ansi(ch: char) -> Option<u8> {
    let cp = ch as u32;
    match cp {
        0x00..=0x7F | 0xA0..=0xFF => Some(cp as u8),
        _ => CP1252_C1
            .iter()
            .position(|&c| c == ch)
            .map(|idx| 0x80 + idx as u8),
    }
}

/// Encode a string to bytes in encoding `enc`. Never writes a BOM (see
/// [`bom_bytes`]). Fails with an [`EncodeError`] rather than substituting
/// a placeholder for an unrepresentable character.
pub fn encode(text: &str, enc: TextEncoding) -> Result<Vec<u8>, EncodeError> {
    match enc {
        TextEncoding::Utf8 => Ok(text.as_bytes().to_vec()),
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
            let little_endian = enc == TextEncoding::Utf16Le;
            let mut out = Vec::with_capacity(text.len() * 2);
            let mut buf = [0u16; 2];
            for ch in text.chars() {
                for &unit in ch.encode_utf16(&mut buf).iter() {
                    if little_endian {
                        out.extend_from_slice(&unit.to_le_bytes());
                    } else {
                        out.extend_from_slice(&unit.to_be_bytes());
                    }
                }
            }
            Ok(out)
        }
        TextEncoding::Ansi => {
            let mut out = Vec::with_capacity(text.len());
            for (position, ch) in text.char_indices() {
                match encode_char_ansi(ch) {
                    Some(byte) => out.push(byte),
                    None => {
                        return Err(EncodeError {
                            position,
                            ch,
                            message: format!(
                                "character '{ch}' (U+{:04X}) cannot be represented in Windows-1252",
                                ch as u32
                            ),
                        })
                    }
                }
            }
            Ok(out)
        }
    }
}

/// Encode a search needle in the document's encoding.
///
/// Search runs on encoded bytes (see `crate::search`), so a needle typed
/// as text must be encoded in the document's encoding first — in
/// particular a UTF-16 document needs UTF-16 needle bytes with the
/// matching byte order.
pub fn encode_needle(text: &str, enc: TextEncoding) -> Result<Vec<u8>, EncodeError> {
    encode(text, enc)
}

// ---------------------------------------------------------------------------
// Extended code pages (roadmap item 29, slice R4c)
// ---------------------------------------------------------------------------

/// An extended code page the owner can choose from the Encoding ▸
/// Code Pages menu. Detection never picks one of these — pages are
/// user-chosen only, because a byte sample cannot reliably tell
/// Shift-JIS from GBK from plain binary.
///
/// Conversion goes through the Win32 `MultiByteToWideChar` /
/// `WideCharToMultiByte` pair (the converter the GUI shell installs
/// at startup — see [`install_page_converter`]), except the two
/// ISO-8859 pages, which have small exact tables in pure Rust so the
/// host test suite covers them end to end.
///
/// Page conversion is LOSSY by design, unlike the strict standard
/// encodings above: decoding replaces a byte sequence the page
/// cannot read with U+FFFD, and encoding a character the page
/// cannot represent fails with [`EncodeError`] at the editor edge
/// (the Win32 call underneath would substitute `'?'`; the shell's
/// converter reports the failure instead, per character, so the
/// editor keeps its "never a silent '?'" rule).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodePage {
    /// Shift-JIS (Windows code page 932).
    ShiftJis,
    /// GBK (Windows code page 936).
    Gbk,
    /// EUC-KR (Windows code page 949).
    EucKr,
    /// ISO-8859-1 (Windows code page 28591).
    Iso88591,
    /// ISO-8859-15 (Windows code page 28605).
    Iso885915,
    /// Windows-1250 (Central European).
    Windows1250,
    /// Windows-1251 (Cyrillic).
    Windows1251,
}

impl CodePage {
    /// Every page, in menu order.
    pub const ALL: [CodePage; 7] = [
        CodePage::ShiftJis,
        CodePage::Gbk,
        CodePage::EucKr,
        CodePage::Iso88591,
        CodePage::Iso885915,
        CodePage::Windows1250,
        CodePage::Windows1251,
    ];

    /// The Windows code-page identifier `MultiByteToWideChar` takes.
    pub fn windows_id(self) -> u32 {
        match self {
            CodePage::ShiftJis => 932,
            CodePage::Gbk => 936,
            CodePage::EucKr => 949,
            CodePage::Iso88591 => 28591,
            CodePage::Iso885915 => 28605,
            CodePage::Windows1250 => 1250,
            CodePage::Windows1251 => 1251,
        }
    }

    /// Short label for the status bar: `"CP932"` and friends.
    pub fn label(self) -> &'static str {
        match self {
            CodePage::ShiftJis => "CP932",
            CodePage::Gbk => "CP936",
            CodePage::EucKr => "CP949",
            CodePage::Iso88591 => "CP28591",
            CodePage::Iso885915 => "CP28605",
            CodePage::Windows1250 => "CP1250",
            CodePage::Windows1251 => "CP1251",
        }
    }

    /// Menu display name: the common name plus the page number.
    pub fn display_name(self) -> &'static str {
        match self {
            CodePage::ShiftJis => "Shift-JIS (932)",
            CodePage::Gbk => "GBK (936)",
            CodePage::EucKr => "EUC-KR (949)",
            CodePage::Iso88591 => "ISO-8859-1 (28591)",
            CodePage::Iso885915 => "ISO-8859-15 (28605)",
            CodePage::Windows1250 => "Windows-1250",
            CodePage::Windows1251 => "Windows-1251",
        }
    }

    /// True when this page converts through the pure-Rust tables
    /// (the ISO-8859 pair) rather than the installed Win32 converter.
    fn is_table_page(self) -> bool {
        matches!(self, CodePage::Iso88591 | CodePage::Iso885915)
    }
}

/// The codec a document is currently viewed and edited through:
/// one of the detected standard encodings, or an owner-chosen
/// extended code page (roadmap item 29). The piece table always
/// stores bytes; the codec only decides how bytes become text at
/// the display / typing edges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextCodec {
    Standard(TextEncoding),
    Page(CodePage),
}

impl TextCodec {
    /// The save-transform unit width for this codec: 2 for the
    /// UTF-16 pair, 1 for everything else (the code pages are
    /// byte-oriented; in the double-byte pages a line-ending byte
    /// never appears inside a multi-byte character, so the 1-byte
    /// transform stays exact).
    pub fn unit_width(self) -> usize {
        match self {
            TextCodec::Standard(TextEncoding::Utf16Le | TextEncoding::Utf16Be) => 2,
            _ => 1,
        }
    }
}

/// The platform page converter the Windows shell installs at
/// startup: byte ↔ UTF-16-unit conversion for one Windows code
/// page, backed by `MultiByteToWideChar` / `WideCharToMultiByte`.
/// It lives behind a hook so this module stays platform-neutral
/// and unsafe-free; the functions themselves live in the shell
/// (one of the crate's sanctioned unsafe modules).
#[derive(Clone, Copy)]
pub struct PageConverter {
    /// Decode bytes in page `page_id` to UTF-16 units, lossily:
    /// sequences the page cannot read become U+FFFD.
    pub decode_units: fn(&[u8], u32) -> Vec<u16>,
    /// Encode ONE character in page `page_id`; `None` when the
    /// page cannot represent it.
    pub encode_char: fn(char, u32) -> Option<Vec<u8>>,
}

static PAGE_CONVERTER: std::sync::OnceLock<PageConverter> = std::sync::OnceLock::new();

/// Install the platform page converter (called once by the Windows
/// shell before any window or headless command runs; later calls
/// are ignored).
pub fn install_page_converter(converter: PageConverter) {
    let _ = PAGE_CONVERTER.set(converter);
}

/// The installed page converter, if any.
pub fn page_converter() -> Option<&'static PageConverter> {
    PAGE_CONVERTER.get()
}

/// ISO-8859-15's eight departures from ISO-8859-1: byte → character.
/// The other 248 bytes are the Latin-1 code point of the same value.
const ISO_8859_15_SPECIALS: [(u8, char); 8] = [
    (0xA4, '\u{20AC}'), // € (Latin-1 has ¤)
    (0xA6, '\u{0160}'), // Š (Latin-1 has ¦)
    (0xA8, '\u{0161}'), // š (Latin-1 has ¨)
    (0xB4, '\u{017D}'), // Ž (Latin-1 has ´)
    (0xB8, '\u{017E}'), // ž (Latin-1 has ¸)
    (0xBC, '\u{0152}'), // Œ (Latin-1 has ¼)
    (0xBD, '\u{0153}'), // œ (Latin-1 has ½)
    (0xBE, '\u{0178}'), // Ÿ (Latin-1 has ¾)
];

/// The character for one byte under an ISO-8859 table page.
fn decode_byte_iso(byte: u8, page: CodePage) -> char {
    if page == CodePage::Iso885915 {
        if let Some(&(_, ch)) = ISO_8859_15_SPECIALS.iter().find(|&&(b, _)| b == byte) {
            return ch;
        }
    }
    byte as char
}

/// The byte for a character under an ISO-8859 table page, or
/// `None` when the page cannot represent it. Exact inverse of
/// [`decode_byte_iso`] — note this makes the eight Latin-1
/// characters ISO-8859-15 displaced (¤ ¦ ¨ ´ ¸ ¼ ½ ¾)
/// unrepresentable in that page, which is the standard's own rule.
fn encode_char_iso(ch: char, page: CodePage) -> Option<u8> {
    if page == CodePage::Iso885915 {
        if let Some(&(byte, _)) = ISO_8859_15_SPECIALS.iter().find(|&&(_, c)| c == ch) {
            return Some(byte);
        }
        // The displaced Latin-1 characters have no 8859-15 byte.
        if matches!(
            ch,
            '\u{00A4}'
                | '\u{00A6}'
                | '\u{00A8}'
                | '\u{00B4}'
                | '\u{00B8}'
                | '\u{00BC}'
                | '\u{00BD}'
                | '\u{00BE}'
        ) {
            return None;
        }
    }
    let cp = ch as u32;
    if cp <= 0xFF {
        Some(cp as u8)
    } else {
        None
    }
}

/// Decode bytes under a code page, lossily (see [`CodePage`]).
/// The ISO-8859 pages decode through their exact tables; the
/// remaining pages go through the installed Win32 converter. With
/// no converter installed (a non-Windows host build — the product
/// itself always installs one at startup) the Win32 pages fall
/// back to the ISO-8859-1 byte mapping so behaviour stays total
/// and documented rather than failing.
pub fn decode_page_lossy(bytes: &[u8], page: CodePage) -> String {
    if page.is_table_page() {
        return bytes.iter().map(|&b| decode_byte_iso(b, page)).collect();
    }
    match page_converter() {
        Some(converter) => {
            let units = (converter.decode_units)(bytes, page.windows_id());
            String::from_utf16_lossy(&units)
        }
        None => bytes.iter().map(|&b| b as char).collect(),
    }
}

/// Encode text under a code page. Fails with [`EncodeError`] for
/// the first character the page cannot represent — never a silent
/// `'?'`. The ISO-8859 pages encode through their exact tables;
/// the remaining pages go through the installed Win32 converter
/// one character at a time (so the error names the character, and
/// no document-sized conversion is ever built here — encoding
/// serves typed text and search needles, which are short). With
/// no converter installed the Win32 pages accept only the
/// ISO-8859-1 range (the documented host-build fallback).
pub fn encode_page(text: &str, page: CodePage) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::with_capacity(text.len());
    for (position, ch) in text.char_indices() {
        let encoded = if page.is_table_page() {
            encode_char_iso(ch, page).map(|b| vec![b])
        } else {
            match page_converter() {
                Some(converter) => (converter.encode_char)(ch, page.windows_id()),
                None => {
                    let cp = ch as u32;
                    if cp <= 0xFF {
                        Some(vec![cp as u8])
                    } else {
                        None
                    }
                }
            }
        };
        match encoded {
            Some(bytes) => out.extend_from_slice(&bytes),
            None => {
                return Err(EncodeError {
                    position,
                    ch,
                    message: format!(
                        "character '{ch}' (U+{:04X}) cannot be represented in {}",
                        ch as u32,
                        page.display_name()
                    ),
                })
            }
        }
    }
    Ok(out)
}

/// Encode text through a [`TextCodec`]: the strict standard
/// [`encode`] for the detected encodings, [`encode_page`] for an
/// owner-chosen page.
pub fn encode_codec(text: &str, codec: TextCodec) -> Result<Vec<u8>, EncodeError> {
    match codec {
        TextCodec::Standard(enc) => encode(text, enc),
        TextCodec::Page(page) => encode_page(text, page),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(text: &str, enc: TextEncoding) -> Vec<u8> {
        encode(text, enc).unwrap()
    }

    // ---- Detection: BOMs ----

    #[test]
    fn detect_boms() {
        let d = detect(&[0xEF, 0xBB, 0xBF, b'h', b'i']);
        assert_eq!(
            d,
            Detection {
                encoding: TextEncoding::Utf8,
                bom_len: 3,
                confident: true
            }
        );
        let d = detect(&[0xFF, 0xFE, b'H', 0x00]);
        assert_eq!(d.encoding, TextEncoding::Utf16Le);
        assert_eq!(d.bom_len, 2);
        assert!(d.confident);
        let d = detect(&[0xFE, 0xFF, 0x00, b'H']);
        assert_eq!(d.encoding, TextEncoding::Utf16Be);
        assert_eq!(d.bom_len, 2);
        assert!(d.confident);
        // A bare BOM is still a BOM.
        assert_eq!(detect(&[0xFF, 0xFE]).encoding, TextEncoding::Utf16Le);
    }

    #[test]
    fn detect_empty_is_utf8_confident() {
        let d = detect(&[]);
        assert_eq!(
            d,
            Detection {
                encoding: TextEncoding::Utf8,
                bom_len: 0,
                confident: true
            }
        );
    }

    // ---- Detection: UTF-8 ----

    #[test]
    fn detect_utf8_ascii_and_multibyte() {
        assert_eq!(detect(b"plain ascii text").encoding, TextEncoding::Utf8);
        let d = detect("héllo wörld — €".as_bytes());
        assert_eq!(d.encoding, TextEncoding::Utf8);
        assert!(d.confident);
        assert_eq!(d.bom_len, 0);
    }

    #[test]
    fn detect_utf8_sample_cut_mid_codepoint() {
        // "café" cut after the first byte of the é sequence (C3 A9).
        let mut sample = b"caf".to_vec();
        sample.push(0xC3);
        assert_eq!(detect(&sample).encoding, TextEncoding::Utf8);
        // A 3-byte character (€ = E2 82 AC) cut after two bytes.
        let mut sample = b"price: ".to_vec();
        sample.extend_from_slice(&[0xE2, 0x82]);
        assert_eq!(detect(&sample).encoding, TextEncoding::Utf8);
        // But genuinely invalid bytes before the tail still flip it.
        let mut sample = vec![0xFF, 0xC3];
        sample.extend_from_slice(b"abc");
        assert_eq!(detect(&sample).encoding, TextEncoding::Ansi);
    }

    // ---- Detection: BOM-less UTF-16 ----

    #[test]
    fn detect_bomless_utf16le_and_be() {
        let le = utf16("Hello world", TextEncoding::Utf16Le);
        let d = detect(&le);
        assert_eq!(d.encoding, TextEncoding::Utf16Le);
        assert_eq!(d.bom_len, 0);
        let be = utf16("Hello world", TextEncoding::Utf16Be);
        assert_eq!(detect(&be).encoding, TextEncoding::Utf16Be);
        // Non-ASCII LE text (é = E9 00) is still caught by the parity rule.
        let le = utf16("Héllo", TextEncoding::Utf16Le);
        assert_eq!(detect(&le).encoding, TextEncoding::Utf16Le);
    }

    #[test]
    fn detect_odd_length_cannot_be_utf16() {
        // UTF-16-looking start, but an odd total length rules UTF-16 out.
        let sample = [b'H', 0x00, b'i', 0x00, b'!'];
        assert_ne!(detect(&sample).encoding, TextEncoding::Utf16Le);
    }

    // ---- Detection: ANSI fallback ----

    #[test]
    fn detect_invalid_utf8_falls_back_to_ansi_not_confident() {
        // 0xE9 followed by 'x' cannot be UTF-8 (0xE9 opens a 3-byte
        // sequence and 'x' is no continuation byte): it is a
        // Windows-1252 é. (A *trailing* 0xE9 would be ambiguous with a
        // cut-off UTF-8 character and is treated as UTF-8 by design.)
        let d = detect(&[b'c', b'a', b'f', 0xE9, b'x']);
        assert_eq!(d.encoding, TextEncoding::Ansi);
        assert!(!d.confident);
        assert_eq!(d.bom_len, 0);
        let d = detect(&[0x80, 0x81, 0xFF, 0x41]);
        assert_eq!(d.encoding, TextEncoding::Ansi);
        assert!(!d.confident);
    }

    // ---- Decode / encode ----

    #[test]
    fn utf8_round_trip() {
        let text = "héllo — € 𝄞";
        let bytes = encode(text, TextEncoding::Utf8).unwrap();
        assert_eq!(bytes, text.as_bytes());
        assert_eq!(decode(&bytes, TextEncoding::Utf8).unwrap(), text);
    }

    #[test]
    fn utf16_round_trip_including_astral_char() {
        // 𝄞 is U+1D11E, a surrogate pair in UTF-16.
        let text = "Aé𝄞z";
        for enc in [TextEncoding::Utf16Le, TextEncoding::Utf16Be] {
            let bytes = encode(text, enc).unwrap();
            assert_eq!(bytes.len(), 10); // (1 + 1 + 2 + 1) code units × 2
            assert_eq!(decode(&bytes, enc).unwrap(), text);
            assert!(decode(&bytes, enc).unwrap().contains('𝄞'));
        }
        // Byte order is real: LE and BE encodings differ and cross-decoding
        // does not accidentally work for this string.
        let le = encode("AB", TextEncoding::Utf16Le).unwrap();
        assert_eq!(le, [0x41, 0x00, 0x42, 0x00]);
        let be = encode("AB", TextEncoding::Utf16Be).unwrap();
        assert_eq!(be, [0x00, 0x41, 0x00, 0x42]);
    }

    #[test]
    fn decode_utf8_error_reports_position() {
        let err = decode(&[b'a', 0xFF, b'b'], TextEncoding::Utf8).unwrap_err();
        assert_eq!(err.position, 1);
    }

    #[test]
    fn decode_utf16_errors_name_positions() {
        // Odd trailing byte.
        let err = decode(&[0x41, 0x00, 0x42], TextEncoding::Utf16Le).unwrap_err();
        assert_eq!(err.position, 2);
        // Lone high surrogate (D800 in LE = 00 D8).
        let err = decode(&[0x00, 0xD8], TextEncoding::Utf16Le).unwrap_err();
        assert_eq!(err.position, 0);
        // Lone low surrogate (DC00 in LE = 00 DC).
        let err = decode(&[0x00, 0xDC], TextEncoding::Utf16Le).unwrap_err();
        assert_eq!(err.position, 0);
        // High surrogate followed by a plain character.
        let err = decode(&[0x00, 0xD8, 0x41, 0x00], TextEncoding::Utf16Le).unwrap_err();
        assert_eq!(err.position, 0);
        // Error position is reported past the valid prefix.
        let err = decode(&[0x41, 0x00, 0x00, 0xD8], TextEncoding::Utf16Le).unwrap_err();
        assert_eq!(err.position, 2);
    }

    #[test]
    fn ansi_round_trip_all_256_bytes() {
        let all: Vec<u8> = (0u8..=255).collect();
        let text = decode(&all, TextEncoding::Ansi).unwrap();
        assert_eq!(text.chars().count(), 256);
        let back = encode(&text, TextEncoding::Ansi).unwrap();
        assert_eq!(back, all);
    }

    #[test]
    fn ansi_table_spot_checks() {
        assert_eq!(decode(&[0x80], TextEncoding::Ansi).unwrap(), "€");
        assert_eq!(decode(&[0x93], TextEncoding::Ansi).unwrap(), "\u{201C}");
        assert_eq!(decode(&[0x97], TextEncoding::Ansi).unwrap(), "—");
        assert_eq!(decode(&[0x9F], TextEncoding::Ansi).unwrap(), "Ÿ");
        assert_eq!(decode(&[0xE9], TextEncoding::Ansi).unwrap(), "é");
        // The five undefined bytes decode to their C1 control characters.
        for byte in [0x81u8, 0x8D, 0x8F, 0x90, 0x9D] {
            let expected = char::from_u32(byte as u32).unwrap();
            assert_eq!(
                decode(&[byte], TextEncoding::Ansi).unwrap(),
                expected.to_string()
            );
        }
    }

    #[test]
    fn encode_ansi_unrepresentable_char_is_an_error() {
        // A CJK character has no Windows-1252 byte: error, never '?'.
        let err = encode("ab\u{4E2D}", TextEncoding::Ansi).unwrap_err();
        assert_eq!(err.ch, '\u{4E2D}');
        assert_eq!(err.position, 2);
        // A C1 control that CP1252 spends on punctuation (U+0085) is also
        // not representable.
        assert!(encode("\u{0085}", TextEncoding::Ansi).is_err());
        // But the five C1 values carried by the undefined bytes encode fine.
        assert_eq!(encode("\u{0081}", TextEncoding::Ansi).unwrap(), [0x81]);
    }

    #[test]
    fn bom_bytes_and_encode_never_writes_bom() {
        assert_eq!(bom_bytes(TextEncoding::Utf8), &[0xEF, 0xBB, 0xBF]);
        assert_eq!(bom_bytes(TextEncoding::Utf16Le), &[0xFF, 0xFE]);
        assert_eq!(bom_bytes(TextEncoding::Utf16Be), &[0xFE, 0xFF]);
        assert_eq!(bom_bytes(TextEncoding::Ansi), &[] as &[u8]);
        let bytes = encode("Hi", TextEncoding::Utf16Le).unwrap();
        assert_eq!(bytes, [0x48, 0x00, 0x69, 0x00]); // no FF FE prefix
    }

    // ---- Extended code pages (roadmap item 29) ----

    #[test]
    fn code_page_ids_and_labels() {
        assert_eq!(CodePage::ALL.len(), 7);
        assert_eq!(CodePage::ShiftJis.windows_id(), 932);
        assert_eq!(CodePage::Gbk.windows_id(), 936);
        assert_eq!(CodePage::EucKr.windows_id(), 949);
        assert_eq!(CodePage::Iso88591.windows_id(), 28591);
        assert_eq!(CodePage::Iso885915.windows_id(), 28605);
        assert_eq!(CodePage::Windows1250.windows_id(), 1250);
        assert_eq!(CodePage::Windows1251.windows_id(), 1251);
        assert_eq!(CodePage::ShiftJis.label(), "CP932");
        assert_eq!(CodePage::Windows1251.label(), "CP1251");
        assert_eq!(CodePage::Iso88591.display_name(), "ISO-8859-1 (28591)");
    }

    #[test]
    fn iso88591_round_trip_all_256_bytes() {
        // The pure-Rust table page, end to end on the host: byte =
        // code point, so all 256 values round-trip exactly.
        let all: Vec<u8> = (0u8..=255).collect();
        let text = decode_page_lossy(&all, CodePage::Iso88591);
        assert_eq!(text.chars().count(), 256);
        assert_eq!(encode_page(&text, CodePage::Iso88591).unwrap(), all);
        assert_eq!(decode_page_lossy(&[0xE9], CodePage::Iso88591), "é");
        // A character past Latin-1 is an EncodeError, never a '?'.
        let err = encode_page("a\u{4E2D}", CodePage::Iso88591).unwrap_err();
        assert_eq!(err.ch, '\u{4E2D}');
        assert_eq!(err.position, 1);
    }

    #[test]
    fn iso885915_swaps_eight_positions() {
        // The eight special positions decode to their 8859-15
        // characters and encode back; everything else is Latin-1.
        for (byte, ch) in ISO_8859_15_SPECIALS {
            assert_eq!(
                decode_page_lossy(&[byte], CodePage::Iso885915),
                ch.to_string()
            );
            assert_eq!(
                encode_page(&ch.to_string(), CodePage::Iso885915).unwrap(),
                [byte]
            );
            // And those same bytes are the plain Latin-1 characters
            // under ISO-8859-1.
            assert_eq!(
                decode_page_lossy(&[byte], CodePage::Iso88591),
                (byte as char).to_string()
            );
        }
        // The displaced Latin-1 characters cannot be encoded in
        // 8859-15 at all (the standard's own rule).
        assert!(encode_page("\u{00A4}", CodePage::Iso885915).is_err());
        assert!(encode_page("\u{00BC}", CodePage::Iso885915).is_err());
        // € exists in both pages but at different bytes.
        assert_eq!(encode_page("€", CodePage::Iso885915).unwrap(), [0xA4]);
        assert!(encode_page("€", CodePage::Iso88591).is_err());
        // Ordinary Latin-1 still round-trips through 8859-15.
        let text = decode_page_lossy(&[0x41, 0xE9, 0xFF], CodePage::Iso885915);
        assert_eq!(text, "Aéÿ");
        assert_eq!(
            encode_page(&text, CodePage::Iso885915).unwrap(),
            [0x41, 0xE9, 0xFF]
        );
    }

    #[test]
    fn win32_pages_without_a_converter_use_the_documented_fallback() {
        // Host builds install no converter: decode falls back to
        // the ISO-8859-1 byte mapping and encode accepts only that
        // range. (On Windows the shell installs the real converter
        // at startup; see gui_win.)
        if page_converter().is_some() {
            return; // another test process state: nothing to pin here
        }
        assert_eq!(decode_page_lossy(&[0x41, 0xE9], CodePage::ShiftJis), "Aé");
        assert_eq!(encode_page("Aé", CodePage::Gbk).unwrap(), [0x41, 0xE9]);
        assert!(encode_page("\u{4E2D}", CodePage::EucKr).is_err());
    }

    #[test]
    fn encode_codec_dispatches_by_variant() {
        assert_eq!(
            encode_codec("ab", TextCodec::Standard(TextEncoding::Utf16Le)).unwrap(),
            [0x61, 0x00, 0x62, 0x00]
        );
        assert_eq!(
            encode_codec("é", TextCodec::Page(CodePage::Iso88591)).unwrap(),
            [0xE9]
        );
        assert_eq!(TextCodec::Standard(TextEncoding::Utf16Be).unit_width(), 2);
        assert_eq!(TextCodec::Page(CodePage::ShiftJis).unit_width(), 1);
    }

    #[test]
    fn encode_needle_matches_document_encoding() {
        assert_eq!(encode_needle("ab", TextEncoding::Utf8).unwrap(), b"ab");
        assert_eq!(
            encode_needle("ab", TextEncoding::Utf16Le).unwrap(),
            [0x61, 0x00, 0x62, 0x00]
        );
        assert_eq!(
            encode_needle("ab", TextEncoding::Utf16Be).unwrap(),
            [0x00, 0x61, 0x00, 0x62]
        );
        assert!(encode_needle("\u{4E2D}", TextEncoding::Ansi).is_err());
    }
}
