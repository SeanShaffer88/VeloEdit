//! The GUI-independent editor state machine.
//!
//! The Windows shell (`crate::gui_win`) is deliberately thin: every
//! editing decision — where the caret is, what is selected, which rows
//! are visible, what a keypress does to the document — lives here, in
//! platform-independent code the host test gate can exercise. The shell
//! only translates Win32 messages into these operations and paints what
//! [`Editor::visible_text_rows`] / [`Editor::visible_hex_rows`] return.
//!
//! Model notes:
//!
//! * The caret is a byte offset in `[0, len]` (an insertion point, in
//!   both modes). In text mode it always rests on a character boundary
//!   after any operation defined here; in hex mode it addresses bytes.
//! * Line splitting is byte-based on `0x0A`, exactly like
//!   [`crate::line_index`]: correct for UTF-8 and ANSI, and for UTF-16
//!   the standard line-feed code unit has `0x0A` as its low byte in both
//!   byte orders. A trailing `'\r'` is treated as part of the line
//!   ending, never as content.
//! * The line index is rebuilt lazily from the piece table whenever its
//!   recorded length no longer matches the document (an edit happened).
//!   On open, the GUI seeds it from its background thread via
//!   [`Editor::attach_line_index`] (the SPEC.md section 3.1 secondary
//!   thread); a rebuild here is a sequential scan, bounded in memory by
//!   the index's 64 KB checkpoint buffer. For a 100+ GB file the seeded
//!   index is what keeps opening instant — the lazy rebuild only runs
//!   after an edit, over the edited document.
//! * Dirty tracking: any mutation sets the flag; undo/redo recompute it
//!   as "history can still undo", which is exact because saving clears
//!   the history (save is a history boundary, Slice 3). One known
//!   wrinkle: if the bounded history ever *evicts* its oldest entries
//!   (1024 by default), undoing everything that remains reports "clean"
//!   even though the document may still differ from the saved file.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::encoding::{self, Detection, EncodeError, TextEncoding};
use crate::hexview;
use crate::line_index::LineIndex;
use crate::piece_table::PieceTable;
use crate::save::{SaveError, SaveReport};
use crate::search::{self, SearchOptions};
use crate::undo::UndoStack;

/// Longest decoded line shown in one viewport row. Longer lines are cut
/// at this many characters and marked — the viewport is virtualised, it
/// never materialises a whole line (SPEC.md section 3.1).
pub const MAX_ROW_CHARS: usize = 4096;

/// Marker appended to a row whose line was truncated for display.
pub const TRUNCATION_MARKER: &str = " \u{2026}"; // " …"

/// Most bytes fetched for one row while locating its end.
const ROW_FETCH_BYTES: u64 = 256 * 1024;

/// Largest text range the sort / dedup line operations will process
/// (roadmap item 23): the range is materialised once to reorder its
/// lines, so it is capped — the rest of the editor never materialises
/// document content at all.
pub const MAX_LINE_OP_BYTES: u64 = 64 * 1024 * 1024;

/// How a document is being viewed/edited.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EditorMode {
    #[default]
    Text,
    Hex,
}

/// Why an editor operation refused to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorError {
    /// The document was opened read-only.
    ReadOnly,
    /// Save was requested for a document with no file path yet.
    NoPath,
    /// Text could not be encoded in the document's encoding.
    Encode(EncodeError),
    /// The atomic save pipeline failed (message names the step).
    Save(String),
    /// A file could not be read.
    Io(String),
    /// A line operation's range expanded past the size limit
    /// (carries the expanded size in bytes).
    RangeTooLarge(u64),
}

impl fmt::Display for EditorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditorError::ReadOnly => write!(f, "the document is read-only"),
            EditorError::NoPath => write!(f, "the document has no file path yet"),
            EditorError::Encode(e) => write!(f, "{e}"),
            EditorError::Save(m) => write!(f, "save failed: {m}"),
            EditorError::Io(m) => write!(f, "{m}"),
            EditorError::RangeTooLarge(n) => write!(
                f,
                "the range covers {n} bytes of text; line operations are limited to {} bytes",
                MAX_LINE_OP_BYTES
            ),
        }
    }
}

impl std::error::Error for EditorError {}

impl From<SaveError> for EditorError {
    fn from(e: SaveError) -> Self {
        EditorError::Save(e.to_string())
    }
}

/// One visible text row: its 0-based line number, the decoded display
/// text (line ending stripped, truncated past [`MAX_ROW_CHARS`]), and
/// the byte range of the line's content in the document (line ending
/// excluded; for line 0 the range starts after any byte-order mark).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextRow {
    pub line_no: u64,
    pub text: String,
    pub start: u64,
    pub end: u64,
}

/// One visible hex row (SPEC.md section 3.2 shape).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HexRow {
    /// Byte offset of the row's first byte.
    pub address: u64,
    /// `"4D 5A 90 …"` — two hex digits per byte, single-space separated,
    /// with an extra space after the eighth byte.
    pub hex: String,
    /// One character per byte: the byte itself when printable ASCII,
    /// `'.'` otherwise.
    pub ascii: String,
    /// How many bytes this row holds (16, or fewer on the last row).
    pub len: u64,
}

/// Everything the status bar shows, computed on demand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusInfo {
    /// 1-based line of the caret.
    pub line: u64,
    /// 1-based character column of the caret within its line.
    pub col: u64,
    /// Byte offset of the caret.
    pub offset: u64,
    /// Document length in bytes.
    pub size: u64,
    pub encoding: TextEncoding,
    pub encoding_confident: bool,
    pub dirty: bool,
    pub read_only: bool,
    pub mode: EditorMode,
    /// Selection length in bytes (0 when nothing is selected).
    pub selection_len: u64,
}

/// Count `'\n'` bytes (the line-splitting byte in every supported
/// encoding's byte representation).
fn count_newlines(bytes: &[u8]) -> i64 {
    bytes.iter().filter(|&&b| b == b'\n').count() as i64
}

/// Decode bytes for *display*: never fails. UTF-8 uses the standard
/// lossy conversion; UTF-16 replaces each malformed unit with U+FFFD
/// and drops a lone trailing byte; ANSI decoding is already total.
pub fn decode_lossy(bytes: &[u8], enc: TextEncoding) -> String {
    match enc {
        TextEncoding::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
        TextEncoding::Ansi => {
            encoding::decode(bytes, enc).expect("ANSI decoding is total by construction")
        }
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
            if let Ok(text) = encoding::decode(bytes, enc) {
                return text;
            }
            let little = enc == TextEncoding::Utf16Le;
            let unit = |i: usize| -> u16 {
                if little {
                    u16::from_le_bytes([bytes[i], bytes[i + 1]])
                } else {
                    u16::from_be_bytes([bytes[i], bytes[i + 1]])
                }
            };
            let mut out = String::new();
            let mut i = 0;
            while i < bytes.len() {
                if i + 1 >= bytes.len() {
                    out.push('\u{FFFD}'); // odd trailing byte
                    break;
                }
                let u = unit(i);
                if (0xD800..=0xDBFF).contains(&u) && i + 3 < bytes.len() {
                    let lo = unit(i + 2);
                    if (0xDC00..=0xDFFF).contains(&lo) {
                        let cp =
                            0x1_0000u32 + (((u as u32) - 0xD800) << 10) + ((lo as u32) - 0xDC00);
                        out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                        i += 4;
                        continue;
                    }
                }
                match char::from_u32(u as u32) {
                    Some(ch) => out.push(ch),
                    None => out.push('\u{FFFD}'),
                }
                i += 2;
            }
            out
        }
    }
}

/// The editor: one document, its history, and the view state over it.
#[derive(Debug)]
pub struct Editor {
    table: PieceTable,
    history: UndoStack,
    detection: Detection,
    path: Option<PathBuf>,
    mode: EditorMode,
    read_only: bool,
    caret: u64,
    anchor: Option<u64>,
    first_line: u64,
    first_row: u64,
    index: Option<LineIndex>,
    /// First byte offset touched by an edit since the index was built
    /// (`None` = the index fully describes the current document). The
    /// index stays *valid* for every offset before this point, which
    /// is what keeps caret/status operations fast after an edit even
    /// in a huge file: they never force a full re-index.
    index_dirty_from: Option<u64>,
    /// Net newline change of all edits since the index was built, so
    /// `line_count` stays exact without a re-index. Marked unknown
    /// after undo/redo (their byte effect is not summarised here).
    nl_delta: i64,
    nl_delta_known: bool,
    /// Last (line, raw start) found by walking, so sequential row
    /// fetches after an edit stay proportional to the viewport.
    walk_cache: Option<(u64, u64)>,
    line_ending: &'static str,
    dirty: bool,
    matches: Vec<u64>,
    match_index: Option<usize>,
    needle_bytes: Vec<u8>,
    needle_text: String,
    find_options: SearchOptions,
}

impl Editor {
    /// A new, empty, untitled document (UTF-8, no path, not dirty).
    pub fn new_empty() -> Editor {
        Editor::from_table(PieceTable::new(0), encoding::detect(&[]), None, false)
    }

    /// Build an editor over an existing piece table — the constructor
    /// the GUI's memory-mapped open path uses (it passes a table backed
    /// by `OriginalBuffer::Mapped` on Windows) and tests use with the
    /// memory backing.
    pub fn from_table(
        table: PieceTable,
        detection: Detection,
        path: Option<PathBuf>,
        read_only: bool,
    ) -> Editor {
        let sample = table.read(0, table.len().min(64 * 1024));
        let line_ending = if sample.windows(2).any(|w| w == b"\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        Editor {
            table,
            history: UndoStack::new(),
            detection,
            path,
            mode: EditorMode::Text,
            read_only,
            caret: 0,
            anchor: None,
            first_line: 0,
            first_row: 0,
            index: None,
            index_dirty_from: None,
            nl_delta: 0,
            nl_delta_known: true,
            walk_cache: None,
            line_ending,
            dirty: false,
            matches: Vec::new(),
            match_index: None,
            needle_bytes: Vec::new(),
            needle_text: String::new(),
            find_options: SearchOptions::default(),
        }
    }

    /// Open an in-memory byte string as a document, detecting the
    /// encoding from its first 64 KB.
    pub fn open_from_bytes(bytes: Vec<u8>, path: Option<PathBuf>, read_only: bool) -> Editor {
        let sample_len = bytes.len().min(64 * 1024);
        let detection = encoding::detect(&bytes[..sample_len]);
        Editor::from_table(PieceTable::from_original(bytes), detection, path, read_only)
    }

    /// Open a file from disk into memory (the host path; the Windows
    /// GUI memory-maps instead and calls [`Editor::from_table`]).
    pub fn open_from_path(path: &Path, read_only: bool) -> Result<Editor, EditorError> {
        let bytes = std::fs::read(path)
            .map_err(|e| EditorError::Io(format!("could not read {}: {e}", path.display())))?;
        Ok(Editor::open_from_bytes(
            bytes,
            Some(path.to_path_buf()),
            read_only,
        ))
    }

    // ---- Accessors -------------------------------------------------

    pub fn table(&self) -> &PieceTable {
        &self.table
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn mode(&self) -> EditorMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: EditorMode) {
        if mode != self.mode {
            self.mode = mode;
            // Keep the caret in view under the new mode's scroll unit.
            if mode == EditorMode::Hex {
                self.first_row = hexview::row_for_offset(self.caret);
            } else {
                let (line, _) = self.line_col_of(self.caret);
                self.first_line = line;
            }
        }
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Mark the document as differing from its file on disk without
    /// recording a history entry. The GUI's crash-recovery path uses
    /// this when it opens recovered content: the bytes are unsaved by
    /// definition, but there is no edit to undo back to.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn caret(&self) -> u64 {
        self.caret
    }

    pub fn total_len(&self) -> u64 {
        self.table.len()
    }

    pub fn detection(&self) -> Detection {
        self.detection
    }

    pub fn encoding(&self) -> TextEncoding {
        self.detection.encoding
    }

    /// The line ending new lines are inserted with: `"\r\n"` when the
    /// document already uses CRLF, `"\n"` otherwise.
    pub fn line_ending(&self) -> &'static str {
        self.line_ending
    }

    pub fn first_line(&self) -> u64 {
        self.first_line
    }

    pub fn first_row(&self) -> u64 {
        self.first_row
    }

    pub fn set_first_line(&mut self, line: u64) {
        let count = self.line_count();
        self.first_line = if count == 0 { 0 } else { line.min(count - 1) };
    }

    pub fn set_first_row(&mut self, row: u64) {
        let rows = hexview::row_count(self.total_len());
        self.first_row = if rows == 0 { 0 } else { row.min(rows - 1) };
    }

    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    /// Replace the undo history with one bounded by the owner's
    /// configured limits (roadmap item 6: a step limit and a byte
    /// budget, from `data/settings.json`). Anything already recorded
    /// is discarded, so the shell calls this on a freshly built
    /// editor, before any edit is made.
    pub fn set_undo_limits(&mut self, steps: usize, byte_budget: u64) {
        self.history = UndoStack::with_limits(steps, byte_budget);
    }

    /// The selected byte range `(start, end)`, or `None` when the
    /// selection is empty (anchor unset or equal to the caret).
    pub fn selection_range(&self) -> Option<(u64, u64)> {
        let anchor = self.anchor?;
        if anchor == self.caret {
            return None;
        }
        Some((anchor.min(self.caret), anchor.max(self.caret)))
    }

    pub fn selected_bytes(&self) -> Vec<u8> {
        match self.selection_range() {
            Some((start, end)) => self.table.read(start, end - start),
            None => Vec::new(),
        }
    }

    /// The selection decoded for the clipboard (lossy — the clipboard
    /// is a display surface; the document bytes are never altered by
    /// copying).
    pub fn selected_text_lossy(&self) -> String {
        decode_lossy(&self.selected_bytes(), self.encoding())
    }

    // ---- Line index ------------------------------------------------

    /// Accept a line index built elsewhere (the GUI's background
    /// thread) when it describes the document's current bytes and the
    /// document has not been edited since; an index for different
    /// bytes is ignored (the lazy paths below keep working anyway).
    pub fn attach_line_index(&mut self, index: LineIndex) {
        if index.total_length() == self.total_len() && self.index_dirty_from.is_none() {
            self.index = Some(index);
            self.nl_delta = 0;
            self.nl_delta_known = true;
            self.walk_cache = None;
        }
    }

    /// Whether the line index fully describes the current document
    /// (built or attached since the last edit).
    pub fn has_current_index(&self) -> bool {
        self.index.is_some() && self.index_dirty_from.is_none()
    }

    fn ensure_index(&mut self) {
        if self.index.is_none() {
            let table = &self.table;
            let index =
                LineIndex::build(self.total_len(), |offset, out| table.read_into(offset, out));
            self.index = Some(index);
            self.index_dirty_from = None;
            self.nl_delta = 0;
            self.nl_delta_known = true;
            self.walk_cache = None;
        }
    }

    /// Force a full re-index now, discarding the lazy stale state.
    /// Editing never needs this (the lazy paths stay exact); it exists
    /// for callers that want the index itself refreshed in one go.
    pub fn rebuild_index(&mut self) {
        self.index = None;
        self.ensure_index();
    }

    /// Number of lines in the document (0 for an empty document).
    /// Exact when the index is current, and exact after edits whose
    /// newline effect is tracked (typing, deletion, replace); after
    /// undo/redo it falls back to the index's own count until a
    /// re-index — a documented approximation only the scrollbar and
    /// page clamping consume.
    pub fn line_count(&mut self) -> u64 {
        self.ensure_index();
        let index = self.index.as_ref().expect("index present");
        if self.index_dirty_from.is_none() {
            return index.line_count();
        }
        if !self.nl_delta_known {
            return index.line_count();
        }
        // Exact from the tracked newline delta, the document's current
        // length, and its current last byte (the same formula
        // LineIndex::line_count applies to a fresh index).
        let total = self.total_len();
        if total == 0 {
            return 0;
        }
        let newlines = (index.total_newlines() as i64 + self.nl_delta).max(0) as u64;
        if self.table.byte_at(total - 1) == Some(b'\n') {
            newlines
        } else {
            newlines + 1
        }
    }

    /// Bounds of the line whose raw content starts at `raw_start`:
    /// `(content_start, content_end, next_line_start)`. The scan stops
    /// at the first `'\n'` (or the end of the document), so the cost
    /// is the line's length, never the document's size.
    fn line_bounds(&self, raw_start: u64) -> (u64, u64, u64) {
        let total = self.total_len();
        let content_start = if raw_start == 0 {
            (self.detection.bom_len as u64).max(raw_start)
        } else {
            raw_start
        };
        let mut pos = raw_start;
        let mut buffer = [0u8; 8192];
        while pos < total {
            let want = ((total - pos).min(8192)) as usize;
            let read = self.table.read_into(pos, &mut buffer[..want]);
            if read == 0 {
                break;
            }
            if let Some(i) = buffer[..read].iter().position(|&b| b == b'\n') {
                let nl = pos + i as u64;
                let mut end = nl;
                if end > raw_start && self.table.byte_at(end - 1) == Some(b'\r') {
                    end -= 1;
                }
                return (content_start, end.max(content_start), nl + 1);
            }
            pos += read as u64;
        }
        (content_start, total.max(content_start), total)
    }

    /// Raw start of `line`: via the index when it is current, and by
    /// walking from the last known-good position when it is stale
    /// (after an edit). Exact either way; the walking cost is the
    /// distance covered, not the document size.
    fn line_start(&mut self, line: u64) -> Option<u64> {
        self.ensure_index();
        if self.index_dirty_from.is_none() {
            let index = self.index.as_ref().expect("index present");
            let table = &self.table;
            let start = index.line_start_offset(line, |offset, out| table.read_into(offset, out));
            if let Some(start) = start {
                self.walk_cache = Some((line, start));
            }
            return start;
        }
        if line == 0 {
            return Some(0);
        }
        // Walking base: the cached position when it is not ahead of
        // the target; otherwise the start of the line containing the
        // first edited byte (that start lies in the index's
        // still-valid prefix — the bytes before an edit never move).
        let dirty = self.index_dirty_from.unwrap_or(0);
        let (mut cur_line, mut cur_start) = match self.walk_cache {
            Some((l, s)) if l <= line => (l, s),
            _ => {
                let (l, start) = self.line_and_start(dirty);
                if line < l {
                    // The target sits entirely inside the valid
                    // prefix: the stale index still answers exactly.
                    let index = self.index.as_ref().expect("index present");
                    let table = &self.table;
                    return index
                        .line_start_offset(line, |offset, out| table.read_into(offset, out));
                }
                (l, start)
            }
        };
        while cur_line < line {
            let (_, _, next) = self.line_bounds(cur_start);
            if next <= cur_start {
                return None; // no further line exists
            }
            cur_line += 1;
            cur_start = next;
            self.walk_cache = Some((cur_line, cur_start));
        }
        Some(cur_start)
    }

    /// Byte range of a line's *content*: line ending excluded, and for
    /// line 0 the range starts after any byte-order mark. `None` when
    /// the line does not exist.
    pub fn line_content_range(&mut self, line: u64) -> Option<(u64, u64)> {
        if self.index_dirty_from.is_none() && line >= self.line_count() {
            return None;
        }
        let raw_start = self.line_start(line)?;
        let (start, end, _) = self.line_bounds(raw_start);
        Some((start, end))
    }

    /// Decoded text of one line's content (strict decode, lossy on
    /// error). `None` when the line does not exist.
    pub fn line_text(&mut self, line: u64) -> Option<String> {
        let (start, end) = self.line_content_range(line)?;
        Some(decode_lossy(
            &self.table.read(start, end - start),
            self.encoding(),
        ))
    }

    /// Number of characters in a line's content.
    pub fn line_char_count(&mut self, line: u64) -> u64 {
        self.line_text(line)
            .map(|t| t.chars().count() as u64)
            .unwrap_or(0)
    }

    /// (0-based line, raw byte start of that line) for a byte offset.
    /// Exact whether or not the index is stale: the index is only
    /// consulted inside its still-valid prefix (the bytes before the
    /// first edited byte never move), and anything past it is found by
    /// scanning forward from there. The caret sits inside that gap
    /// right after an edit, so the scan is normally a few bytes; a
    /// caret far past an edit in a huge file costs one scan of the
    /// gap, after which the walk cache keeps repeats cheap.
    fn line_and_start(&mut self, offset: u64) -> (u64, u64) {
        let offset = offset.min(self.total_len());
        self.ensure_index();
        let index = self.index.as_ref().expect("index present");
        // Deepest offset the index still describes exactly.
        let valid_upto = match self.index_dirty_from {
            None => offset,
            Some(dirty) => offset.min(dirty),
        };
        let cp = index.nearest_checkpoint(valid_upto);
        // The walk cache may offer a later, equally valid seed.
        let (mut line, mut line_start, mut pos) = if let Some((wl, ws)) = self
            .walk_cache
            .filter(|(_, ws)| *ws > cp.byte_offset && *ws <= offset)
        {
            (wl, Some(ws), ws)
        } else {
            (
                cp.line_number,
                if cp.at_line_start {
                    Some(cp.byte_offset)
                } else {
                    None
                },
                cp.byte_offset,
            )
        };
        let mut buffer = [0u8; 8192];
        while pos < offset {
            let want = ((offset - pos).min(8192)) as usize;
            let read = self.table.read_into(pos, &mut buffer[..want]);
            if read == 0 {
                break;
            }
            for (i, &b) in buffer[..read].iter().enumerate() {
                if b == b'\n' {
                    line += 1;
                    line_start = Some(pos + i as u64 + 1);
                }
            }
            pos += read as u64;
        }
        let start = match line_start {
            Some(s) => s,
            None => {
                // The line began before the seed: scan backwards for
                // its start. Only a very long single line walks far.
                let mut start = 0;
                let mut hi = cp.byte_offset;
                'outer: while hi > 0 {
                    let lo = hi.saturating_sub(65536);
                    let data = self.table.read(lo, hi - lo);
                    for (i, &b) in data.iter().enumerate().rev() {
                        if b == b'\n' {
                            start = lo + i as u64 + 1;
                            break 'outer;
                        }
                    }
                    hi = lo;
                }
                start
            }
        };
        (line, start)
    }

    /// The (0-based line, 0-based character column) of a byte offset.
    /// An offset inside a character reports the column of that
    /// character's start; an offset inside a line ending reports the
    /// end of the line's content.
    pub fn line_col_of(&mut self, offset: u64) -> (u64, u64) {
        let offset = offset.min(self.total_len());
        let (line, raw_start) = self.line_and_start(offset);
        let (content_start, content_end, _) = self.line_bounds(raw_start);
        let col = if offset <= content_start {
            0
        } else {
            let upto = offset.min(content_end);
            decode_lossy(
                &self.table.read(content_start, upto - content_start),
                self.encoding(),
            )
            .chars()
            .count() as u64
        };
        (line, col)
    }

    /// Byte offset of (line, character column), clamped to the line's
    /// content. `None` when the line does not exist.
    pub fn offset_of_line_col(&mut self, line: u64, col: u64) -> Option<u64> {
        let text = self.line_text(line)?;
        let (start, _) = self.line_content_range(line)?;
        let prefix: String = text.chars().take(col as usize).collect();
        match encoding::encode(&prefix, self.encoding()) {
            Ok(bytes) => Some(start + bytes.len() as u64),
            Err(_) => {
                // The decoded prefix holds a character this encoding
                // cannot write back (possible after a lossy decode of
                // damaged bytes): fall back to one byte per column,
                // clamped into the content range.
                let (_, end) = self.line_content_range(line)?;
                Some((start + col).min(end))
            }
        }
    }

    // ---- Viewport rows ----------------------------------------------

    /// The visible text rows starting at `first_line`, at most `count`.
    pub fn visible_text_rows(&mut self, first_line: u64, count: usize) -> Vec<TextRow> {
        let total = self.line_count();
        let mut rows = Vec::new();
        let mut line = first_line;
        while (rows.len()) < count && line < total {
            let Some((start, end)) = self.line_content_range(line) else {
                break;
            };
            let fetch_len = (end - start).min(ROW_FETCH_BYTES);
            let bytes = self.table.read(start, fetch_len);
            let mut text = decode_lossy(&bytes, self.encoding());
            let truncated = fetch_len < end - start || text.chars().count() > MAX_ROW_CHARS;
            if text.chars().count() > MAX_ROW_CHARS {
                text = text.chars().take(MAX_ROW_CHARS).collect();
            }
            if truncated {
                text.push_str(TRUNCATION_MARKER);
            }
            rows.push(TextRow {
                line_no: line,
                text,
                start,
                end,
            });
            line += 1;
        }
        rows
    }

    /// The visible hex rows starting at `first_row`, at most `count`.
    pub fn visible_hex_rows(&self, first_row: u64, count: usize) -> Vec<HexRow> {
        let total_rows = hexview::row_count(self.total_len());
        let mut rows = Vec::new();
        let mut row = first_row;
        while rows.len() < count && row < total_rows {
            let address = hexview::row_offset(row);
            let len = hexview::BYTES_PER_ROW.min(self.total_len() - address);
            let bytes = self.table.read(address, len);
            let mut hex = String::new();
            let mut ascii = String::new();
            for (i, &b) in bytes.iter().enumerate() {
                if i > 0 {
                    hex.push(' ');
                    if i == 8 {
                        hex.push(' ');
                    }
                }
                hex.push_str(&format!("{b:02X}"));
                ascii.push(if (0x20..=0x7E).contains(&b) {
                    b as char
                } else {
                    '.'
                });
            }
            rows.push(HexRow {
                address,
                hex,
                ascii,
                len,
            });
            row += 1;
        }
        rows
    }

    // ---- Caret and selection ----------------------------------------

    fn set_caret(&mut self, offset: u64, extend_selection: bool) {
        let offset = offset.min(self.total_len());
        if extend_selection {
            if self.anchor.is_none() {
                self.anchor = Some(self.caret);
            }
        } else {
            self.anchor = None;
        }
        self.caret = offset;
    }

    /// Move the caret to a byte offset (clamped). With
    /// `extend_selection` the anchor stays where it was (or is set to
    /// the old caret), growing/shrinking the selection.
    pub fn move_caret_to(&mut self, offset: u64, extend_selection: bool) {
        self.set_caret(offset, extend_selection);
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.caret = self.total_len();
        if self.caret == 0 {
            self.anchor = None;
        }
    }

    pub fn move_left(&mut self, extend: bool) {
        match self.mode {
            EditorMode::Hex => {
                let target = self.caret.saturating_sub(1);
                self.set_caret(target, extend);
            }
            EditorMode::Text => {
                if !extend {
                    if let Some((start, _)) = self.selection_range() {
                        self.set_caret(start, false);
                        return;
                    }
                }
                let (line, col) = self.line_col_of(self.caret);
                if col > 0 {
                    if let Some(off) = self.offset_of_line_col(line, col - 1) {
                        self.set_caret(off, extend);
                    }
                } else if line > 0 {
                    if let Some((_, end)) = self.line_content_range(line - 1) {
                        self.set_caret(end, extend);
                    }
                }
            }
        }
    }

    pub fn move_right(&mut self, extend: bool) {
        match self.mode {
            EditorMode::Hex => {
                let target = (self.caret + 1).min(self.total_len());
                self.set_caret(target, extend);
            }
            EditorMode::Text => {
                if !extend {
                    if let Some((_, end)) = self.selection_range() {
                        self.set_caret(end, false);
                        return;
                    }
                }
                let (line, col) = self.line_col_of(self.caret);
                let chars = self.line_char_count(line);
                if col < chars {
                    if let Some(off) = self.offset_of_line_col(line, col + 1) {
                        self.set_caret(off, extend);
                    }
                } else if let Some((start, _)) = self.line_content_range(line + 1) {
                    self.set_caret(start, extend);
                }
            }
        }
    }

    pub fn move_up(&mut self, extend: bool) {
        self.move_vertical(-1, extend);
    }

    pub fn move_down(&mut self, extend: bool) {
        self.move_vertical(1, extend);
    }

    fn move_vertical(&mut self, delta: i64, extend: bool) {
        match self.mode {
            EditorMode::Hex => {
                let step = hexview::BYTES_PER_ROW as i64 * delta;
                let target = (self.caret as i64 + step).clamp(0, self.total_len() as i64);
                self.set_caret(target as u64, extend);
            }
            EditorMode::Text => {
                let count = self.line_count();
                if count == 0 {
                    return;
                }
                let (line, col) = self.line_col_of(self.caret);
                let target_line = (line as i64 + delta).clamp(0, count as i64 - 1) as u64;
                if let Some(off) = self.offset_of_line_col(target_line, col) {
                    self.set_caret(off, extend);
                }
            }
        }
    }

    pub fn move_page_up(&mut self, page_rows: u64, extend: bool) {
        self.move_vertical(-(page_rows.max(1) as i64), extend);
    }

    pub fn move_page_down(&mut self, page_rows: u64, extend: bool) {
        self.move_vertical(page_rows.max(1) as i64, extend);
    }

    pub fn move_home(&mut self, extend: bool) {
        match self.mode {
            EditorMode::Hex => {
                let row = hexview::row_for_offset(self.caret);
                self.set_caret(hexview::row_offset(row), extend);
            }
            EditorMode::Text => {
                let (line, _) = self.line_col_of(self.caret);
                if let Some((start, _)) = self.line_content_range(line) {
                    self.set_caret(start, extend);
                }
            }
        }
    }

    pub fn move_end(&mut self, extend: bool) {
        match self.mode {
            EditorMode::Hex => {
                let row = hexview::row_for_offset(self.caret);
                let end = hexview::row_offset(row)
                    .saturating_add(hexview::BYTES_PER_ROW)
                    .min(self.total_len());
                self.set_caret(end, extend);
            }
            EditorMode::Text => {
                let (line, _) = self.line_col_of(self.caret);
                if let Some((_, end)) = self.line_content_range(line) {
                    self.set_caret(end, extend);
                }
            }
        }
    }

    pub fn move_doc_start(&mut self, extend: bool) {
        self.set_caret(0, extend);
    }

    pub fn move_doc_end(&mut self, extend: bool) {
        let end = self.total_len();
        self.set_caret(end, extend);
    }

    /// Move the caret to 1-based line `line_1based` (the `--line`
    /// convention), clamped to the document, and scroll it into view.
    pub fn goto_line(&mut self, line_1based: u64) {
        let count = self.line_count();
        if count == 0 {
            self.set_caret(0, false);
            return;
        }
        let line = line_1based.saturating_sub(1).min(count - 1);
        if let Some((start, _)) = self.line_content_range(line) {
            self.set_caret(start, false);
            self.first_line = line;
        }
    }

    /// Adjust the scroll position so the caret is inside the visible
    /// window of `visible_rows` rows (call after any caret move).
    pub fn ensure_caret_visible(&mut self, visible_rows: u64) {
        let rows = visible_rows.max(1);
        match self.mode {
            EditorMode::Hex => {
                let row = hexview::row_for_offset(self.caret);
                if row < self.first_row {
                    self.first_row = row;
                } else if row >= self.first_row + rows {
                    self.first_row = row + 1 - rows;
                }
            }
            EditorMode::Text => {
                let (line, _) = self.line_col_of(self.caret);
                if line < self.first_line {
                    self.first_line = line;
                } else if line >= self.first_line + rows {
                    self.first_line = line + 1 - rows;
                }
            }
        }
    }

    // ---- Editing ------------------------------------------------------

    /// Bookkeeping after any document mutation: the dirty flag, the
    /// find state, and the line index's stale marker. `first_touched`
    /// is the first byte offset the edit changed; the index stays
    /// valid before it (see `index_dirty_from`).
    fn after_mutation(&mut self, first_touched: u64) {
        self.dirty = true;
        self.matches.clear();
        self.match_index = None;
        self.index_dirty_from = Some(match self.index_dirty_from {
            Some(d) => d.min(first_touched),
            None => first_touched,
        });
        self.walk_cache = None;
    }

    /// Track the newline effect of an edit so `line_count` stays exact
    /// without a re-index. No-op once the delta is unknown (undo/redo).
    fn note_newline_delta(&mut self, inserted: &[u8], removed: &[u8]) {
        if self.nl_delta_known {
            self.nl_delta += count_newlines(inserted) - count_newlines(removed);
        }
    }

    fn require_writable(&self) -> Result<(), EditorError> {
        if self.read_only {
            Err(EditorError::ReadOnly)
        } else {
            Ok(())
        }
    }

    /// Insert raw bytes at the caret, replacing the selection when one
    /// exists — always exactly one undo entry. This is the hex-mode /
    /// paste-bytes path; text typing goes through
    /// [`Editor::insert_text`], which encodes first.
    pub fn insert_bytes(&mut self, bytes: &[u8]) -> Result<(), EditorError> {
        self.require_writable()?;
        if bytes.is_empty() {
            return Ok(());
        }
        if let Some((start, end)) = self.selection_range() {
            let removed = self.table.read(start, end - start);
            self.note_newline_delta(bytes, &removed);
            search::replace_one(
                &mut self.table,
                &mut self.history,
                start,
                end - start,
                bytes,
            );
            self.caret = start + bytes.len() as u64;
            self.anchor = None;
            self.after_mutation(start);
        } else {
            let at = self.caret;
            self.note_newline_delta(bytes, &[]);
            self.history.do_insert(&mut self.table, at, bytes);
            self.caret += bytes.len() as u64;
            self.anchor = None;
            self.after_mutation(at);
        }
        Ok(())
    }

    /// Type text at the caret: encoded in the document's encoding
    /// first, so an unrepresentable character is an error and nothing
    /// is inserted (never a silent `'?'`).
    pub fn insert_text(&mut self, text: &str) -> Result<(), EditorError> {
        self.require_writable()?;
        let bytes = encoding::encode(text, self.encoding()).map_err(EditorError::Encode)?;
        self.insert_bytes(&bytes)
    }

    /// Insert a line break using the document's line ending.
    pub fn insert_newline(&mut self) -> Result<(), EditorError> {
        let ending = self.line_ending;
        self.insert_text(ending)
    }

    /// Delete the current selection as one undo entry. Returns whether
    /// anything was deleted.
    pub fn delete_selection(&mut self) -> Result<bool, EditorError> {
        self.require_writable()?;
        let Some((start, end)) = self.selection_range() else {
            return Ok(false);
        };
        let removed = self.table.read(start, end - start);
        self.note_newline_delta(&[], &removed);
        self.history.do_delete(&mut self.table, start, end - start);
        self.caret = start;
        self.anchor = None;
        self.after_mutation(start);
        Ok(true)
    }

    /// Backspace: delete the selection, or the character (text mode) /
    /// byte (hex mode) before the caret, as one undo entry.
    pub fn backspace(&mut self) -> Result<bool, EditorError> {
        self.require_writable()?;
        if self.selection_range().is_some() {
            return self.delete_selection();
        }
        if self.caret == 0 {
            return Ok(false);
        }
        let start = match self.mode {
            EditorMode::Hex => self.caret - 1,
            EditorMode::Text => {
                let (line, col) = self.line_col_of(self.caret);
                if col > 0 {
                    self.offset_of_line_col(line, col - 1)
                        .unwrap_or(self.caret - 1)
                } else if line > 0 {
                    // Delete the line ending itself: from the previous
                    // line's content end to this line's content start.
                    match self.line_content_range(line - 1) {
                        Some((_, prev_end)) => prev_end,
                        None => self.caret - 1,
                    }
                } else {
                    return Ok(false);
                }
            }
        };
        let start = start.min(self.caret);
        let removed = self.table.read(start, self.caret - start);
        self.note_newline_delta(&[], &removed);
        self.history
            .do_delete(&mut self.table, start, self.caret - start);
        self.caret = start;
        self.anchor = None;
        self.after_mutation(start);
        Ok(true)
    }

    /// Delete key: delete the selection, or the character / byte after
    /// the caret, as one undo entry.
    pub fn delete_forward(&mut self) -> Result<bool, EditorError> {
        self.require_writable()?;
        if self.selection_range().is_some() {
            return self.delete_selection();
        }
        if self.caret >= self.total_len() {
            return Ok(false);
        }
        let end = match self.mode {
            EditorMode::Hex => (self.caret + 1).min(self.total_len()),
            EditorMode::Text => {
                let (line, col) = self.line_col_of(self.caret);
                let chars = self.line_char_count(line);
                if col < chars {
                    self.offset_of_line_col(line, col + 1)
                        .unwrap_or((self.caret + 1).min(self.total_len()))
                } else {
                    // Delete the line ending: to the next line's
                    // content start.
                    match self.line_content_range(line + 1) {
                        Some((next_start, _)) => next_start,
                        None => (self.caret + 1).min(self.total_len()),
                    }
                }
            }
        };
        let end = end.max(self.caret + 1).min(self.total_len());
        let removed = self.table.read(self.caret, end - self.caret);
        self.note_newline_delta(&[], &removed);
        let at = self.caret;
        self.history
            .do_delete(&mut self.table, self.caret, end - self.caret);
        self.anchor = None;
        self.after_mutation(at);
        Ok(true)
    }

    /// Overwrite the byte at the caret with `byte` (hex-mode typing),
    /// as one undo entry; at the very end of the document this appends.
    pub fn overwrite_byte(&mut self, byte: u8) -> Result<(), EditorError> {
        self.require_writable()?;
        let at = self.caret;
        if at < self.total_len() {
            let old = self.table.byte_at(at).unwrap_or(0);
            self.note_newline_delta(&[byte], &[old]);
            search::replace_one(&mut self.table, &mut self.history, at, 1, &[byte]);
        } else {
            self.note_newline_delta(&[byte], &[]);
            self.history.do_insert(&mut self.table, at, &[byte]);
        }
        self.caret = (at + 1).min(self.total_len());
        self.anchor = None;
        self.after_mutation(at);
        Ok(())
    }

    /// Undo one edit. Returns whether anything was undone. The caret
    /// stays where it is (clamped into the restored document).
    pub fn undo(&mut self) -> bool {
        if !self.history.apply_undo(&mut self.table) {
            return false;
        }
        self.caret = self.caret.min(self.total_len());
        self.anchor = None;
        self.matches.clear();
        self.match_index = None;
        // The restored bytes can differ anywhere: the index is stale
        // from the start, and the newline delta is unknown.
        self.index_dirty_from = Some(0);
        self.nl_delta_known = false;
        self.walk_cache = None;
        self.dirty = self.history.can_undo();
        true
    }

    /// Redo one undone edit.
    pub fn redo(&mut self) -> bool {
        if !self.history.apply_redo(&mut self.table) {
            return false;
        }
        self.caret = self.caret.min(self.total_len());
        self.anchor = None;
        self.matches.clear();
        self.match_index = None;
        self.index_dirty_from = Some(0);
        self.nl_delta_known = false;
        self.walk_cache = None;
        self.dirty = true;
        true
    }

    // ---- Find and replace ----------------------------------------------

    /// Run a find over the whole document and select the first match
    /// at or after the caret (wrapping to the first match). UTF-16
    /// matches are filtered to code-unit-aligned offsets (the
    /// straddling-match caveat in `docs/PARITY.md`). Returns the number
    /// of matches.
    pub fn set_find(
        &mut self,
        needle_text: &str,
        options: SearchOptions,
    ) -> Result<usize, EditorError> {
        let needle =
            encoding::encode_needle(needle_text, self.encoding()).map_err(EditorError::Encode)?;
        self.needle_bytes = needle;
        self.needle_text = needle_text.to_string();
        self.find_options = options;
        self.refresh_matches();
        if self.matches.is_empty() {
            self.match_index = None;
            return Ok(0);
        }
        let idx = self
            .matches
            .iter()
            .position(|&m| m >= self.caret)
            .unwrap_or(0);
        self.select_match(idx);
        Ok(self.matches.len())
    }

    /// Run a find limited to the byte range `[start, end)` and select
    /// the first match at or after the caret (wrapping to the first
    /// match in the range) — the "search in selection" surface over
    /// the scoped core (roadmap item 11). Only matches lying fully
    /// inside the range are ever in the match list, so
    /// [`Editor::find_next`] / [`Editor::find_prev`] walk the range
    /// and nothing outside it. The UTF-16 alignment filter of
    /// [`Editor::set_find`] applies the same way. Returns the number
    /// of matches in the range.
    pub fn set_find_in_range(
        &mut self,
        needle_text: &str,
        options: SearchOptions,
        start: u64,
        end: u64,
    ) -> Result<usize, EditorError> {
        let needle =
            encoding::encode_needle(needle_text, self.encoding()).map_err(EditorError::Encode)?;
        self.needle_bytes = needle;
        self.needle_text = needle_text.to_string();
        self.find_options = options;
        let mut found =
            search::find_in_range(&self.table, &self.needle_bytes, &options, start, end);
        if matches!(
            self.encoding(),
            TextEncoding::Utf16Le | TextEncoding::Utf16Be
        ) {
            let bom = self.detection.bom_len as u64;
            found.retain(|&m| m >= bom && (m - bom) % 2 == 0);
        }
        self.matches = found;
        if self.matches.is_empty() {
            self.match_index = None;
            return Ok(0);
        }
        let idx = self
            .matches
            .iter()
            .position(|&m| m >= self.caret)
            .unwrap_or(0);
        self.select_match(idx);
        Ok(self.matches.len())
    }

    /// Run a masked byte-pattern find (roadmap item 12) and select the
    /// first match at or after the caret, wrapping like
    /// [`Editor::set_find`]. The match list and
    /// [`Editor::current_match`] behave exactly as for a text find —
    /// the stored needle is a stand-in of the pattern's length, which
    /// is all the selection and highlight maths use — so the hex
    /// surface drives the same caret/selection flow. The saved needle
    /// *text* is empty on purpose: a masked pattern is not text, and
    /// an edit that clears the match list must not silently re-run it
    /// as one. Returns the number of matches.
    pub fn set_find_masked(&mut self, pattern: &search::MaskedPattern) -> usize {
        self.needle_bytes = vec![0u8; pattern.len()];
        self.needle_text = String::new();
        self.find_options = SearchOptions::default();
        self.matches = search::find_masked_all(&self.table, pattern, None);
        if self.matches.is_empty() {
            self.match_index = None;
            return 0;
        }
        let idx = self
            .matches
            .iter()
            .position(|&m| m >= self.caret)
            .unwrap_or(0);
        self.select_match(idx);
        self.matches.len()
    }

    fn refresh_matches(&mut self) {
        if self.needle_bytes.is_empty() {
            self.matches.clear();
            return;
        }
        let mut found = search::find_all(&self.table, &self.needle_bytes, &self.find_options);
        if matches!(
            self.encoding(),
            TextEncoding::Utf16Le | TextEncoding::Utf16Be
        ) {
            let bom = self.detection.bom_len as u64;
            found.retain(|&m| m >= bom && (m - bom) % 2 == 0);
        }
        self.matches = found;
    }

    fn select_match(&mut self, idx: usize) {
        if let Some(&start) = self.matches.get(idx) {
            self.match_index = Some(idx);
            self.anchor = Some(start);
            self.caret = start + self.needle_bytes.len() as u64;
        }
    }

    /// The current match's byte range, if one is selected.
    pub fn current_match(&self) -> Option<(u64, u64)> {
        let idx = self.match_index?;
        let start = *self.matches.get(idx)?;
        Some((start, start + self.needle_bytes.len() as u64))
    }

    /// All current match offsets (recomputed by [`Editor::set_find`];
    /// cleared by any edit).
    pub fn matches(&self) -> &[u64] {
        &self.matches
    }

    /// 0-based index of the current match within [`Editor::matches`].
    pub fn match_index(&self) -> Option<usize> {
        self.match_index
    }

    /// Select the next match, wrapping around the end. Re-runs the
    /// last find when the match list was cleared by an edit.
    pub fn find_next(&mut self) -> Option<(u64, u64)> {
        if self.matches.is_empty() {
            if self.needle_text.is_empty() {
                return None;
            }
            let text = self.needle_text.clone();
            let options = self.find_options;
            let _ = self.set_find(&text, options);
            return self.current_match();
        }
        let next = match self.match_index {
            Some(i) => (i + 1) % self.matches.len(),
            None => 0,
        };
        self.select_match(next);
        self.current_match()
    }

    /// Select the previous match, wrapping around the start.
    pub fn find_prev(&mut self) -> Option<(u64, u64)> {
        if self.matches.is_empty() {
            if self.needle_text.is_empty() {
                return None;
            }
            let text = self.needle_text.clone();
            let options = self.find_options;
            let _ = self.set_find(&text, options);
            return self.current_match();
        }
        let prev = match self.match_index {
            Some(0) | None => self.matches.len() - 1,
            Some(i) => i - 1,
        };
        self.select_match(prev);
        self.current_match()
    }

    /// Replace the currently selected match with `replacement`, then
    /// select the next match. Returns whether a replacement happened.
    pub fn replace_current(&mut self, replacement: &str) -> Result<bool, EditorError> {
        self.require_writable()?;
        let Some((start, end)) = self.current_match() else {
            return Ok(false);
        };
        // Only replace when the selection really is that match (the
        // user may have clicked elsewhere since it was selected).
        if self.selection_range() != Some((start, end)) {
            return Ok(false);
        }
        let bytes = encoding::encode(replacement, self.encoding()).map_err(EditorError::Encode)?;
        let removed = self.table.read(start, end - start);
        self.note_newline_delta(&bytes, &removed);
        search::replace_one(
            &mut self.table,
            &mut self.history,
            start,
            end - start,
            &bytes,
        );
        self.caret = start + bytes.len() as u64;
        self.anchor = None;
        self.after_mutation(start);
        // Re-run the same find and select the next match from here.
        let text = self.needle_text.clone();
        let options = self.find_options;
        let _ = self.set_find(&text, options);
        Ok(true)
    }

    /// Replace every occurrence of the last find needle with
    /// `replacement`, as one undo entry. Returns the number replaced.
    pub fn replace_all(&mut self, replacement: &str) -> Result<usize, EditorError> {
        self.require_writable()?;
        if self.needle_bytes.is_empty() {
            return Ok(0);
        }
        let bytes = encoding::encode(replacement, self.encoding()).map_err(EditorError::Encode)?;
        let needle = self.needle_bytes.clone();
        let options = self.find_options;
        let first = search::find_next(&self.table, &needle, 0, &options).unwrap_or(0);
        let report = search::replace_all(
            &mut self.table,
            &mut self.history,
            &needle,
            &bytes,
            &options,
        );
        if report.count > 0 {
            // Every replaced needle leaves the count by (newlines in
            // the replacement − newlines in the needle) per match.
            if self.nl_delta_known {
                self.nl_delta +=
                    report.count as i64 * (count_newlines(&bytes) - count_newlines(&needle));
            }
            self.caret = self.caret.min(self.total_len());
            self.anchor = None;
            self.after_mutation(first);
        }
        Ok(report.count)
    }

    /// Replace every occurrence of the last find needle lying fully
    /// inside `[start, end)` with `replacement`, as one undo entry
    /// (roadmap item 11). Returns the number replaced.
    pub fn replace_all_in_range(
        &mut self,
        replacement: &str,
        start: u64,
        end: u64,
    ) -> Result<usize, EditorError> {
        self.require_writable()?;
        if self.needle_bytes.is_empty() || start >= end {
            return Ok(0);
        }
        let bytes = encoding::encode(replacement, self.encoding()).map_err(EditorError::Encode)?;
        let needle = self.needle_bytes.clone();
        let options = self.find_options;
        let first = search::find_in_range(&self.table, &needle, &options, start, end)
            .first()
            .copied();
        let report = search::replace_all_in_range(
            &mut self.table,
            &mut self.history,
            &needle,
            &bytes,
            &options,
            start,
            end,
        );
        if report.count > 0 {
            if self.nl_delta_known {
                self.nl_delta +=
                    report.count as i64 * (count_newlines(&bytes) - count_newlines(&needle));
            }
            self.caret = self.caret.min(self.total_len());
            self.anchor = None;
            self.after_mutation(first.unwrap_or(start));
        }
        Ok(report.count)
    }

    // ---- Line operations (roadmap item 23) ---------------------------

    /// Expand `[start, end)` to whole lines: returns the byte range
    /// covering every line the range touches. Line 0 starts after any
    /// byte-order mark, so the mark is never sorted away from the top.
    fn expand_to_lines(&mut self, start: u64, end: u64) -> (u64, u64) {
        let (_, raw_start) = self.line_and_start(start);
        let region_start = if raw_start == 0 {
            (self.detection.bom_len as u64).max(raw_start)
        } else {
            raw_start
        };
        let (_, last_start) = self.line_and_start(end - 1);
        let (_, _, next) = self.line_bounds(last_start);
        (region_start, next)
    }

    /// Sort the lines touched by `[start, end)` (expanded to whole
    /// lines), byte-wise and stable, as ONE undoable edit. Each line
    /// keeps its own terminator, so CRLF lines stay CRLF. Returns the
    /// number of lines in the region. Refuses — with no change — when
    /// the expanded region exceeds [`MAX_LINE_OP_BYTES`].
    pub fn sort_lines_in_range(&mut self, start: u64, end: u64) -> Result<usize, EditorError> {
        let (lines, _) = self.transform_lines_in_range(start, end, false)?;
        Ok(lines)
    }

    /// Remove duplicate lines within `[start, end)` (expanded to
    /// whole lines), keeping the first occurrence of each line, as
    /// ONE undoable edit. Lines compare by content (terminator
    /// excluded). Returns the number of lines removed. Same size
    /// limit and refusal behaviour as [`Editor::sort_lines_in_range`].
    pub fn dedup_lines_in_range(&mut self, start: u64, end: u64) -> Result<usize, EditorError> {
        let (_, removed) = self.transform_lines_in_range(start, end, true)?;
        Ok(removed)
    }

    /// Shared body of the sort / dedup operations: returns
    /// (lines in the region, lines removed by dedup — always 0 for a
    /// sort).
    fn transform_lines_in_range(
        &mut self,
        start: u64,
        end: u64,
        dedup: bool,
    ) -> Result<(usize, usize), EditorError> {
        self.require_writable()?;
        if end <= start || start >= self.total_len() {
            return Ok((0, 0));
        }
        let end = end.min(self.total_len());
        if end - start > MAX_LINE_OP_BYTES {
            return Err(EditorError::RangeTooLarge(end - start));
        }
        let (rs, re) = self.expand_to_lines(start, end);
        if re - rs > MAX_LINE_OP_BYTES {
            return Err(EditorError::RangeTooLarge(re - rs));
        }
        if re <= rs {
            return Ok((0, 0));
        }
        let old = self.table.read(rs, re - rs);
        // Split into lines that keep their own terminators (the last
        // line may have none).
        let chunks: Vec<&[u8]> = old.split_inclusive(|&b| b == b'\n').collect();
        let line_count = chunks.len();
        let (new_chunks, removed): (Vec<&[u8]>, usize) = if dedup {
            let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
            let mut kept: Vec<&[u8]> = Vec::with_capacity(line_count);
            let mut removed = 0usize;
            for chunk in &chunks {
                let mut key = *chunk;
                if key.last() == Some(&b'\n') {
                    key = &key[..key.len() - 1];
                }
                if key.last() == Some(&b'\r') {
                    key = &key[..key.len() - 1];
                }
                if seen.insert(key.to_vec()) {
                    kept.push(chunk);
                } else {
                    removed += 1;
                }
            }
            (kept, removed)
        } else {
            let mut sorted = chunks.clone();
            sorted.sort(); // stable, byte-wise on the line bytes
            (sorted, 0)
        };
        let new: Vec<u8> = new_chunks.concat();
        if new == old {
            return Ok((line_count, 0));
        }
        self.note_newline_delta(&new, &old);
        search::replace_one(&mut self.table, &mut self.history, rs, re - rs, &new);
        self.caret = self.caret.min(self.total_len());
        self.anchor = None;
        self.after_mutation(rs);
        Ok((line_count, removed))
    }

    // ---- Save -----------------------------------------------------------

    /// Save over the document's own file with the atomic pipeline
    /// (SPEC.md section 5.1). Saving clears the history (it is a
    /// history boundary) and the dirty flag. A line index that is
    /// current stays valid (saving does not change the bytes); a stale
    /// one described the pre-edit bytes and is discarded.
    pub fn save(&mut self) -> Result<SaveReport, EditorError> {
        self.require_writable_save()?;
        let path = self.path.clone().ok_or(EditorError::NoPath)?;
        let report = self.table.save_to(&path, &mut self.history)?;
        Ok(self.finish_save(report))
    }

    /// Save to a new path (Save As): the path is adopted only when the
    /// save succeeds. Unlike [`Editor::save`], Save As is allowed for
    /// a read-only document — writing the (unedited) content to a
    /// *different* file is exactly how a read-only view gets kept.
    pub fn save_as(&mut self, path: &Path) -> Result<SaveReport, EditorError> {
        let report = self.table.save_to(path, &mut self.history)?;
        self.path = Some(path.to_path_buf());
        Ok(self.finish_save(report))
    }

    /// Stream the current selection straight to a new file at `dest`
    /// (roadmap item 35), in chunks, with no clipboard involved — a
    /// selection of any size can be exported. Returns the number of
    /// bytes written; an empty selection writes an empty file and
    /// returns 0 (the shell only offers the command when a selection
    /// exists, so this is just the honest core behaviour).
    pub fn save_selection_to(&self, dest: &Path) -> std::io::Result<u64> {
        match self.selection_range() {
            Some((start, end)) => {
                crate::save::write_range_to_file(&self.table, start, end - start, dest)
            }
            None => crate::save::write_range_to_file(&self.table, 0, 0, dest),
        }
    }

    fn require_writable_save(&self) -> Result<(), EditorError> {
        if self.read_only {
            return Err(EditorError::ReadOnly);
        }
        Ok(())
    }

    fn finish_save(&mut self, report: SaveReport) -> SaveReport {
        self.dirty = false;
        self.matches.clear();
        self.match_index = None;
        self.caret = self.caret.min(self.total_len());
        if self.index_dirty_from.is_some() {
            // The stale index described the pre-edit document; the
            // saved file holds the edited bytes, so it must go. (A
            // current index stays valid — saving changes no bytes.)
            self.index = None;
            self.index_dirty_from = None;
            self.nl_delta = 0;
            self.nl_delta_known = true;
            self.walk_cache = None;
        }
        report
    }

    // ---- Status ---------------------------------------------------------

    /// Everything the status bar shows.
    pub fn status(&mut self) -> StatusInfo {
        let (line, col) = self.line_col_of(self.caret);
        StatusInfo {
            line: line + 1,
            col: col + 1,
            offset: self.caret,
            size: self.total_len(),
            encoding: self.encoding(),
            encoding_confident: self.detection.confident,
            dirty: self.dirty,
            read_only: self.read_only,
            mode: self.mode,
            selection_len: self.selection_range().map(|(s, e)| e - s).unwrap_or(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::SearchOptions;

    fn ed(bytes: &[u8]) -> Editor {
        Editor::open_from_bytes(bytes.to_vec(), None, false)
    }

    fn content(e: &Editor) -> Vec<u8> {
        e.table().read(0, e.total_len())
    }

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "veloedit-editor-test-{}-{name}",
            std::process::id()
        ));
        p
    }

    #[test]
    fn open_type_undo_redo_save_round_trip() {
        let path = temp_path("roundtrip.txt");
        std::fs::write(&path, b"hello").unwrap();
        let mut e = Editor::open_from_path(&path, false).unwrap();
        assert!(!e.is_dirty());
        assert_eq!(e.total_len(), 5);

        e.move_doc_end(false);
        e.insert_text(" world").unwrap();
        assert_eq!(content(&e), b"hello world");
        assert!(e.is_dirty());

        assert!(e.undo());
        assert_eq!(content(&e), b"hello");
        assert!(!e.is_dirty(), "undo back to the opened state is clean");

        assert!(e.redo());
        assert_eq!(content(&e), b"hello world");
        assert!(e.is_dirty());

        e.save().unwrap();
        assert!(!e.is_dirty());
        assert!(!e.can_undo(), "save is a history boundary");
        assert_eq!(std::fs::read(&path).unwrap(), b"hello world");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn save_as_adopts_the_path_and_keeps_the_original() {
        let src = temp_path("as-src.txt");
        let dst = temp_path("as-dst.txt");
        std::fs::write(&src, b"data").unwrap();
        let mut e = Editor::open_from_path(&src, false).unwrap();
        e.move_doc_end(false);
        e.insert_text("!").unwrap();
        e.save_as(&dst).unwrap();
        assert_eq!(e.path(), Some(dst.as_path()));
        assert_eq!(std::fs::read(&dst).unwrap(), b"data!");
        assert_eq!(std::fs::read(&src).unwrap(), b"data");
        std::fs::remove_file(&src).ok();
        std::fs::remove_file(&dst).ok();
    }

    #[test]
    fn save_selection_writes_exactly_the_selected_bytes() {
        let dst = temp_path("sel-out.bin");
        let mut e = ed(b"hello world");
        for _ in 0..5 {
            e.move_right(true);
        }
        assert_eq!(e.selection_range(), Some((0, 5)));
        let written = e.save_selection_to(&dst).unwrap();
        assert_eq!(written, 5);
        assert_eq!(std::fs::read(&dst).unwrap(), b"hello");
        std::fs::remove_file(&dst).ok();
    }

    #[test]
    fn save_selection_with_no_selection_writes_an_empty_file() {
        let dst = temp_path("sel-none.bin");
        let e = ed(b"hello");
        let written = e.save_selection_to(&dst).unwrap();
        assert_eq!(written, 0);
        assert_eq!(std::fs::read(&dst).unwrap(), b"");
        std::fs::remove_file(&dst).ok();
    }

    #[test]
    fn configured_undo_limits_apply_to_the_editor() {
        // An 8-byte delta budget: the first insert weighs 4 (2 bytes
        // after + 2 action bytes), the coalesced second weighs 8 —
        // together 12 > 8, so the oldest step is trimmed and only the
        // second insert can be undone.
        let mut e = ed(b"");
        e.set_undo_limits(1024, 8);
        e.insert_text("ab").unwrap();
        e.insert_text("cd").unwrap();
        assert_eq!(content(&e), b"abcd");
        assert!(e.undo());
        assert_eq!(content(&e), b"ab");
        assert!(!e.undo());
    }

    #[test]
    fn caret_steps_over_multibyte_characters() {
        // "aé\nxy": é is two bytes in UTF-8.
        let mut e = ed("a\u{00E9}\nxy".as_bytes());
        assert_eq!(e.total_len(), 6);
        e.move_right(false);
        assert_eq!(e.caret(), 1);
        e.move_right(false);
        assert_eq!(e.caret(), 3, "one step over the two-byte character");
        e.move_right(false);
        assert_eq!(e.caret(), 4, "start of line 1");
        e.move_right(false);
        assert_eq!(e.caret(), 5, "one character into line 1");
        e.move_left(false);
        assert_eq!(e.caret(), 4);
        let (line, col) = e.line_col_of(e.caret());
        assert_eq!((line, col), (1, 0));
    }

    #[test]
    fn home_end_up_down_and_status() {
        let mut e = ed(b"abc\nde\nfghi");
        e.goto_line(3);
        assert_eq!(e.caret(), 7);
        e.move_end(false);
        assert_eq!(e.caret(), 11);
        e.move_home(false);
        assert_eq!(e.caret(), 7);
        e.move_up(false);
        assert_eq!(e.caret(), 4, "same column on the shorter line above");
        let st = e.status();
        assert_eq!((st.line, st.col, st.offset), (2, 1, 4));
        e.move_page_down(10, false);
        let (line, _) = e.line_col_of(e.caret());
        assert_eq!(line, 2);
    }

    #[test]
    fn selection_replace_is_one_undo_entry() {
        let mut e = ed(b"hello world");
        e.move_caret_to(6, false);
        e.move_caret_to(11, true);
        assert_eq!(e.selection_range(), Some((6, 11)));
        assert_eq!(e.selected_text_lossy(), "world");
        e.insert_text("there").unwrap();
        assert_eq!(content(&e), b"hello there");
        assert_eq!(e.caret(), 11);
        assert!(e.undo());
        assert_eq!(content(&e), b"hello world", "one undo restores both halves");
    }

    #[test]
    fn backspace_deletes_char_then_newline_then_char() {
        let mut e = ed(b"ab\ncd");
        e.move_doc_end(false);
        assert!(e.backspace().unwrap());
        assert_eq!(content(&e), b"ab\nc");
        e.move_caret_to(3, false); // start of line 1 ("c" was at 3..4, now line "c")
                                   // Caret at start of line 1: backspace removes the newline.
        assert_eq!(e.line_col_of(e.caret()), (1, 0));
        assert!(e.backspace().unwrap());
        assert_eq!(content(&e), b"abc");
        assert!(e.backspace().unwrap());
        assert_eq!(content(&e), b"ac", "the character before the caret goes");
    }

    #[test]
    fn delete_forward_at_line_end_joins_lines() {
        let mut e = ed(b"ab\ncd");
        e.move_caret_to(2, false); // end of line 0
        assert!(e.delete_forward().unwrap());
        assert_eq!(content(&e), b"abcd");
        assert_eq!(e.caret(), 2);
    }

    #[test]
    fn hex_rows_are_formatted_with_address_hex_and_ascii() {
        let mut data = b"\x4D\x5A\x00\x01hello world!\x7F\x80".to_vec();
        data.extend_from_slice(&[0x41; 4]);
        let e = ed(&data);
        let rows = e.visible_hex_rows(0, 4);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].address, 0);
        assert_eq!(rows[0].len, 16);
        assert!(
            rows[0].hex.starts_with("4D 5A 00 01 68 65 6C 6C"),
            "{}",
            rows[0].hex
        );
        assert_eq!(rows[0].ascii, "MZ..hello world!");
        assert_eq!(rows[1].address, 16);
        assert_eq!(rows[1].ascii, "..AAAA");
    }

    #[test]
    fn hex_mode_caret_and_overwrite() {
        let mut e = ed(&[0x00, 0x11, 0x22, 0x33]);
        e.set_mode(EditorMode::Hex);
        e.move_right(false);
        e.move_right(false);
        assert_eq!(e.caret(), 2);
        e.overwrite_byte(0x99).unwrap();
        assert_eq!(content(&e), vec![0x00, 0x11, 0x99, 0x33]);
        assert_eq!(e.caret(), 3);
        assert!(e.undo());
        assert_eq!(content(&e), vec![0x00, 0x11, 0x22, 0x33]);
        e.move_down(false);
        assert_eq!(e.caret(), 4, "down clamps to the end of a short file");
        e.move_home(false);
        assert_eq!(e.caret(), 0);
    }

    #[test]
    fn visible_text_rows_decode_and_mark_long_lines() {
        let mut bytes = b"one\ntwo\n".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', 5000));
        bytes.extend_from_slice(b"\nend");
        let mut e = Editor::open_from_bytes(bytes, None, false);
        let rows = e.visible_text_rows(0, 10);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].text, "one");
        assert_eq!((rows[0].start, rows[0].end), (0, 3));
        assert_eq!(rows[1].text, "two");
        let long = &rows[2];
        assert!(long.text.ends_with(TRUNCATION_MARKER));
        assert!(long.text.chars().count() <= MAX_ROW_CHARS + TRUNCATION_MARKER.chars().count());
        assert_eq!(rows[3].text, "end");
        // Rows past the end are not produced.
        assert!(e.visible_text_rows(4, 3).is_empty());
    }

    #[test]
    fn crlf_lines_have_no_carriage_return_in_content() {
        let mut e = ed(b"ab\r\ncd\r\n");
        assert_eq!(e.line_ending(), "\r\n");
        let (start, end) = e.line_content_range(0).unwrap();
        assert_eq!((start, end), (0, 2));
        assert_eq!(e.line_text(0).unwrap(), "ab");
        e.goto_line(2);
        e.move_end(false);
        e.insert_newline().unwrap();
        let text = String::from_utf8(content(&e)).unwrap();
        assert!(text.contains("cd\r\n\r\n"), "{text:?}");
    }

    #[test]
    fn find_navigation_wraps_and_replace_works() {
        let mut e = ed(b"aXbXcX");
        let count = e.set_find("X", SearchOptions::default()).unwrap();
        assert_eq!(count, 3);
        assert_eq!(e.current_match(), Some((1, 2)));
        assert_eq!(e.find_next(), Some((3, 4)));
        assert_eq!(e.find_next(), Some((5, 6)));
        assert_eq!(e.find_next(), Some((1, 2)), "wraps to the first match");
        assert_eq!(e.find_prev(), Some((5, 6)), "and back around");

        assert!(e.replace_current("YY").unwrap());
        assert_eq!(content(&e), b"aXbXcYY");
        let replaced = e.replace_all("z").unwrap();
        assert_eq!(replaced, 2);
        assert_eq!(content(&e), b"azbzcYY");
        assert!(e.undo(), "replace-all is one history entry");
        assert_eq!(content(&e), b"aXbXcYY");
    }

    #[test]
    fn find_in_utf16_document_uses_aligned_matches_only() {
        let text = "aXbX";
        let mut bytes = vec![0xFF, 0xFE];
        for u in text.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let mut e = Editor::open_from_bytes(bytes, None, false);
        assert_eq!(e.encoding(), TextEncoding::Utf16Le);
        let count = e.set_find("X", SearchOptions::default()).unwrap();
        assert_eq!(count, 2);
        assert_eq!(e.current_match(), Some((4, 6)));
        // Typing inserts UTF-16 code units.
        e.move_doc_end(false);
        e.insert_text("!").unwrap();
        let all = content(&e);
        assert_eq!(
            crate::encoding::decode(&all[2..], TextEncoding::Utf16Le).unwrap(),
            "aXbX!"
        );
    }

    #[test]
    fn replace_all_in_range_scopes_to_the_selection() {
        let mut e = ed(b"foo bar foo baz foo");
        e.set_find("foo", SearchOptions::default()).unwrap();
        // Replace only inside the middle of the document.
        let n = e.replace_all_in_range("quux", 4, 15).unwrap();
        assert_eq!(n, 1);
        assert_eq!(content(&e), b"foo bar quux baz foo");
        assert!(e.undo(), "scoped replace-all is one history entry");
        assert_eq!(content(&e), b"foo bar foo baz foo");
    }

    #[test]
    fn find_in_range_scopes_navigation_to_the_range() {
        let mut e = ed(b"foo bar foo baz foo");
        // Scope to the middle of the document: only the match at
        // offset 8 is inside 4..15.
        let count = e
            .set_find_in_range("foo", SearchOptions::default(), 4, 15)
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(e.matches(), &[8]);
        assert_eq!(e.current_match(), Some((8, 11)));
        // Navigation wraps inside the scoped list, never outside it.
        assert_eq!(e.find_next(), Some((8, 11)));
        // A wider scope sees its own matches only.
        let count = e
            .set_find_in_range("foo", SearchOptions::default(), 0, 12)
            .unwrap();
        assert_eq!(count, 2);
        assert_eq!(e.matches(), &[0, 8]);
    }

    #[test]
    fn masked_find_drives_the_match_flow() {
        let mut e = Editor::open_from_bytes(
            vec![0x4A, 0x11, 0xFF, 0x00, 0x2B, 0x99, 0x4A, 0x77, 0xFF],
            None,
            false,
        );
        let pattern = search::parse_masked_pattern("4A ?? FF").unwrap();
        let count = e.set_find_masked(&pattern);
        assert_eq!(count, 2);
        assert_eq!(e.matches(), &[0, 6]);
        // The current match spans the pattern's full length, so the
        // selection is the matched bytes.
        assert_eq!(e.current_match(), Some((0, 3)));
        assert_eq!(e.selection_range(), Some((0, 3)));
        assert_eq!(e.find_next(), Some((6, 9)));
        assert_eq!(e.find_next(), Some((0, 3)), "wraps like a text find");
        // A pattern with no occurrence leaves no current match.
        let pattern = search::parse_masked_pattern("DE AD").unwrap();
        assert_eq!(e.set_find_masked(&pattern), 0);
        assert_eq!(e.current_match(), None);
    }

    #[test]
    fn sort_lines_in_range_sorts_whole_lines_as_one_undo() {
        let mut e = ed(b"banana\napple\ncherry\n");
        // A partial range expands to the whole lines it touches
        // (bytes 2..14 touch all three lines).
        let n = e.sort_lines_in_range(2, 14).unwrap();
        assert_eq!(n, 3);
        assert_eq!(content(&e), b"apple\nbanana\ncherry\n");
        assert!(e.undo());
        assert_eq!(content(&e), b"banana\napple\ncherry\n");
        assert!(!e.can_undo(), "the sort was a single history entry");
    }

    #[test]
    fn sort_lines_preserves_crlf_terminators() {
        let mut e = ed(b"b\r\na\r\nc\r\n");
        let n = e.sort_lines_in_range(0, e.total_len()).unwrap();
        assert_eq!(n, 3);
        assert_eq!(content(&e), b"a\r\nb\r\nc\r\n");
    }

    #[test]
    fn dedup_lines_keeps_first_occurrences_as_one_undo() {
        let mut e = ed(b"x\ny\nx\nz\ny\n");
        let removed = e.dedup_lines_in_range(0, e.total_len()).unwrap();
        assert_eq!(removed, 2);
        assert_eq!(content(&e), b"x\ny\nz\n");
        assert!(e.undo());
        assert_eq!(content(&e), b"x\ny\nx\nz\ny\n");
        assert!(!e.can_undo());
        // Dedup of an already-unique range changes and records nothing.
        let removed = e.dedup_lines_in_range(0, 3).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn line_ops_refuse_an_oversized_range_without_changing_anything() {
        // A document past the 64 MB line-op limit (built as one big
        // allocation of short lines; the refusal must come before any
        // edit is recorded).
        let mut bytes = Vec::new();
        while (bytes.len() as u64) <= MAX_LINE_OP_BYTES {
            bytes.extend_from_slice(b"line\n");
        }
        let mut e = Editor::open_from_bytes(bytes, None, false);
        let total = e.total_len();
        assert_eq!(
            e.sort_lines_in_range(0, total),
            Err(EditorError::RangeTooLarge(total))
        );
        assert_eq!(
            e.dedup_lines_in_range(0, total),
            Err(EditorError::RangeTooLarge(total))
        );
        assert!(!e.can_undo());
        assert!(!e.is_dirty());
    }

    #[test]
    fn read_only_refuses_edits_but_not_reading() {
        let mut e = Editor::open_from_bytes(b"abc".to_vec(), None, true);
        assert_eq!(e.insert_text("x"), Err(EditorError::ReadOnly));
        assert_eq!(e.backspace(), Err(EditorError::ReadOnly));
        assert_eq!(e.delete_forward(), Err(EditorError::ReadOnly));
        assert_eq!(e.save(), Err(EditorError::ReadOnly));
        assert!(!e.is_dirty());
        assert_eq!(e.visible_text_rows(0, 1)[0].text, "abc");
        assert_eq!(e.total_len(), 3);
    }

    #[test]
    fn dirty_flag_transitions() {
        let mut e = ed(b"");
        assert!(!e.is_dirty());
        e.insert_text("x").unwrap();
        assert!(e.is_dirty());
        assert!(e.undo());
        assert!(!e.is_dirty());
        assert!(e.redo());
        assert!(e.is_dirty());
    }

    #[test]
    fn goto_line_clamps_and_scrolls() {
        let mut e = ed(b"l1\nl2\nl3");
        e.goto_line(99);
        assert_eq!(e.caret(), 6);
        assert_eq!(e.first_line(), 2);
        e.goto_line(1);
        assert_eq!(e.caret(), 0);
    }

    #[test]
    fn decode_lossy_replaces_bad_utf16_units() {
        // Lone high surrogate between two ASCII units.
        let bytes = [0x41, 0x00, 0x00, 0xD8, 0x42, 0x00];
        let text = decode_lossy(&bytes, TextEncoding::Utf16Le);
        assert_eq!(text, "A\u{FFFD}B");
        assert_eq!(decode_lossy(&[0x61], TextEncoding::Utf16Le), "\u{FFFD}");
    }

    #[test]
    fn empty_document_behaviour() {
        let mut e = Editor::new_empty();
        assert_eq!(e.total_len(), 0);
        assert_eq!(e.line_count(), 0);
        assert!(e.visible_text_rows(0, 5).is_empty());
        assert!(e.visible_hex_rows(0, 5).is_empty());
        assert_eq!(e.save(), Err(EditorError::NoPath));
        e.insert_text("first").unwrap();
        assert_eq!(e.line_count(), 1);
        assert_eq!(e.line_text(0).unwrap(), "first");
    }

    #[test]
    fn stale_index_stays_exact_for_counts_lines_and_caret() {
        let mut e = ed(b"a\nb");
        assert_eq!(e.line_count(), 2);
        assert!(e.has_current_index());
        e.move_doc_end(false);
        e.insert_text("\nc").unwrap();
        assert!(!e.has_current_index(), "edit staled the index");
        // The count stays exact through the tracked newline delta,
        // lines past the edit walk exactly, and the caret position
        // resolves through the valid prefix + a short scan.
        assert_eq!(e.line_count(), 3);
        assert!(!e.has_current_index(), "no silent rebuild happened");
        assert_eq!(e.line_text(2).unwrap(), "c");
        assert_eq!(e.line_text(0).unwrap(), "a");
        assert_eq!(e.line_col_of(e.caret()), (2, 1));
        // An explicit rebuild restores a current index.
        e.rebuild_index();
        assert!(e.has_current_index());
        assert_eq!(e.line_count(), 3);
    }

    #[test]
    fn line_positions_stay_exact_after_a_mid_document_edit() {
        let mut e = ed(b"one\ntwo\nthree\nfour");
        assert_eq!(e.line_count(), 4);
        // Edit line 1 ("two" -> "two\ninserted"): caret math, later
        // lines and the count must all stay exact without a rebuild.
        e.goto_line(2);
        e.move_end(false);
        e.insert_text("\ninserted").unwrap();
        assert!(!e.has_current_index());
        assert_eq!(e.line_count(), 5);
        assert_eq!(e.line_text(2).unwrap(), "inserted");
        assert_eq!(e.line_text(3).unwrap(), "three");
        assert_eq!(e.line_text(4).unwrap(), "four");
        let (line, col) = e.line_col_of(e.caret());
        assert_eq!((line, col), (2, 8));
        e.goto_line(5);
        assert_eq!(e.line_text(4).unwrap(), "four");
        // Deleting the newline again rejoins the lines exactly.
        e.goto_line(3);
        e.move_home(false);
        assert!(e.backspace().unwrap());
        assert_eq!(e.line_count(), 4);
        assert_eq!(e.line_text(1).unwrap(), "twoinserted");
    }
}
