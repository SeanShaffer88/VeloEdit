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
//! * The line index is *staged*: the first line question over a fresh
//!   document indexes only the head region
//!   ([`crate::line_index::INDEX_HEAD_BYTES`]) synchronously, so
//!   opening a huge file never waits for a full scan, and the GUI's
//!   background thread (the SPEC.md section 3.1 secondary thread)
//!   delivers the growing and then complete index through
//!   [`Editor::attach_line_index`]. While the index is partial,
//!   positions stay exact — lines past the frontier resolve by
//!   scanning forward from the nearest checkpoint — and
//!   [`Editor::line_count`] reports the indexed prefix's count, a
//!   lower bound, never a claimed document total.
//!   [`Editor::rebuild_index`] is the explicit full synchronous
//!   build. After an edit the index is stale rather than rebuilt:
//!   its still-valid prefix plus bounded forward scans keep every
//!   answer exact (see `index_dirty_from`).
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
use crate::line_index::{count_newlines_in, LineIndex, INDEX_HEAD_BYTES};
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

/// What [`Editor::note_file_growth`] changed (roadmap item 22,
/// Ghost Tail core).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GrowthReport {
    /// Document (and original store) length before the growth.
    pub old_len: u64,
    /// Document (and original store) length after the growth.
    pub new_len: u64,
    /// Bytes the file grew by.
    pub added: u64,
    /// The line index's frontier after the growth: the grown index
    /// is partial with this many bytes indexed, and the background
    /// build resumes from exactly here.
    pub index_frontier: u64,
}

/// Why [`Editor::note_file_growth`] refused. Every refusal leaves
/// the editor untouched; the caller routes refused growths to the
/// ordinary external-change path (reload prompt), which is also
/// where shrinks and same-size changes belong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrowthError {
    /// The file did not grow: `new_len` is at or below the current
    /// length (a shrink or a same-size content change).
    NotGrowth { current: u64, requested: u64 },
    /// The document has been edited since it was opened (or carries
    /// undo/redo history): grown bytes are only unambiguous in a
    /// document that still is the file, byte for byte.
    EditedDocument,
    /// The piece table refused the extension (its tail is not the
    /// file's tail, the tail bytes did not fit the backing, or the
    /// backing is a mapping the shell must re-map first).
    Backing(crate::piece_table::ExtendOriginalError),
}

impl fmt::Display for GrowthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GrowthError::NotGrowth { current, requested } => write!(
                f,
                "the file did not grow (it is {current} bytes, growth was claimed to {requested})"
            ),
            GrowthError::EditedDocument => write!(
                f,
                "the document has unsaved edits, so grown bytes cannot be told apart from them"
            ),
            GrowthError::Backing(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for GrowthError {}

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
    /// Hex display layout (roadmap items 13/14): row width,
    /// grouping, and group byte order. Purely presentational —
    /// caret offsets and edits stay byte-based under every layout.
    hex_layout: hexview::HexLayout,
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
    /// Memo of the last line start resolved under a partial
    /// (staged) index, keyed by line. Paint passes ask for the
    /// first visible line on every frame while the walk cache sits
    /// a page ahead of it; without this memo, a view parked past
    /// the frontier (say at end of file) would re-scan the whole
    /// unindexed gap per frame. Cleared wherever `walk_cache` is.
    row_anchor: Option<(u64, u64)>,
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
            hex_layout: hexview::HexLayout::default(),
            index: None,
            index_dirty_from: None,
            nl_delta: 0,
            nl_delta_known: true,
            walk_cache: None,
            row_anchor: None,
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
                self.first_row = hexview::row_for_offset_w(self.caret, self.hex_width());
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
        let rows = hexview::row_count_w(self.total_len(), self.hex_width());
        self.first_row = if rows == 0 { 0 } else { row.min(rows - 1) };
    }

    /// The current hex display layout.
    pub fn hex_layout(&self) -> hexview::HexLayout {
        self.hex_layout
    }

    /// Row width of the current hex layout (the layout's
    /// `bytes_per_row`, never zero).
    fn hex_width(&self) -> u64 {
        self.hex_layout.bytes_per_row.max(1)
    }

    /// Set the hex display layout. Changing the row width re-derives
    /// the scroll row from the caret when the caret's new row would
    /// fall above the viewport; the caret itself never moves.
    pub fn set_hex_layout(&mut self, layout: hexview::HexLayout) {
        if layout == self.hex_layout {
            return;
        }
        let width_changed = layout.bytes_per_row != self.hex_layout.bytes_per_row;
        self.hex_layout = layout;
        if width_changed {
            let rows = hexview::row_count_w(self.total_len(), self.hex_width());
            self.first_row = if rows == 0 {
                0
            } else {
                self.first_row.min(rows - 1)
            };
            let caret_row = hexview::row_for_offset_w(self.caret, self.hex_width());
            if caret_row < self.first_row {
                self.first_row = caret_row;
            }
        }
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
    /// Staged builds arrive as a growing sequence of snapshots: a
    /// snapshot whose frontier is behind the one already held is
    /// ignored, so the indexed prefix never shrinks.
    pub fn attach_line_index(&mut self, index: LineIndex) {
        if index.total_length() == self.total_len() && self.index_dirty_from.is_none() {
            if let Some(current) = &self.index {
                if current.indexed_bytes() > index.indexed_bytes() {
                    return;
                }
            }
            self.index = Some(index);
            self.nl_delta = 0;
            self.nl_delta_known = true;
            self.walk_cache = None;
            self.row_anchor = None;
        }
    }

    /// Whether the line index fully describes the current document
    /// (built or attached since the last edit).
    pub fn has_current_index(&self) -> bool {
        self.index.is_some() && self.index_dirty_from.is_none()
    }

    /// Whether a *complete* line index describes the current
    /// document: the staged background build has finished and no
    /// edit has landed since. Callers that need document totals
    /// (a go-to-line clamp, the final line count) wait for this.
    pub fn has_complete_index(&self) -> bool {
        self.index.as_ref().is_some_and(|i| i.is_complete()) && self.index_dirty_from.is_none()
    }

    /// Whether the held index is a staged build still in progress
    /// (its checkpoints cover a prefix of the document only).
    pub fn index_is_partial(&self) -> bool {
        self.index.as_ref().is_some_and(|i| !i.is_complete())
    }

    /// Bytes covered by the held index (0 when there is none): the
    /// staged frontier the background build has reached.
    pub fn index_frontier(&self) -> u64 {
        self.index.as_ref().map(|i| i.indexed_bytes()).unwrap_or(0)
    }

    /// Index the head region now (the staged-open seed): the same
    /// lazy build the first line question would perform, done
    /// explicitly at open so the first paint never pays for it.
    /// Documents no larger than the head region come back fully
    /// indexed; larger ones are partial until the background build
    /// is attached.
    pub fn seed_line_index(&mut self) {
        self.ensure_index();
    }

    fn ensure_index(&mut self) {
        if self.index.is_none() {
            // Staged open: only the head region is indexed
            // synchronously, so the first line question over a huge
            // document returns at once. Completing the index is the
            // caller's background job (the GUI's index thread,
            // delivered via `attach_line_index`) or an explicit
            // `rebuild_index`; until then, positions resolve exactly
            // through the partial index's forward scans and
            // `line_count` reports the indexed prefix.
            let table = &self.table;
            let index =
                LineIndex::build_prefix(self.total_len(), INDEX_HEAD_BYTES, |offset, out| {
                    table.read_into(offset, out)
                });
            self.index = Some(index);
            self.index_dirty_from = None;
            self.nl_delta = 0;
            self.nl_delta_known = true;
            self.walk_cache = None;
            self.row_anchor = None;
        }
    }

    /// A snapshot of the line index this editor currently holds, for
    /// a background builder that wants to RESUME a build (Ghost Tail:
    /// after growth adoption, the held index describes the grown
    /// document's prefix and `LineIndexBuilder::resume_from` can
    /// continue from its frontier). `None` when no untampered index
    /// is held — the caller then builds from scratch.
    pub fn line_index_snapshot(&self) -> Option<crate::line_index::LineIndex> {
        if self.index_dirty_from.is_some() {
            return None;
        }
        self.index.clone()
    }

    /// Build the structural-filter view of the document (roadmap
    /// item 18): the ordered lines in which a match of `query`
    /// starts, over the current line index. On a still-partial index
    /// the view covers only the indexed prefix and says so
    /// (`FilterView::is_complete` is false).
    pub fn filter_view(
        &mut self,
        query: crate::filter::FilterQuery<'_>,
    ) -> Option<crate::filter::FilterView> {
        self.ensure_index();
        let index = self.index.as_ref()?;
        Some(crate::filter::build_filter(&self.table, index, query))
    }

    /// Force a full re-index now, discarding the lazy stale state.
    /// Editing never needs this (the lazy paths stay exact); it exists
    /// for callers that want the index itself refreshed in one go.
    pub fn rebuild_index(&mut self) {
        let table = &self.table;
        let index = LineIndex::build(self.total_len(), |offset, out| table.read_into(offset, out));
        self.index = Some(index);
        self.index_dirty_from = None;
        self.nl_delta = 0;
        self.nl_delta_known = true;
        self.walk_cache = None;
        self.row_anchor = None;
    }

    /// Note that the file on disk GREW to `new_len` bytes by
    /// appends (roadmap item 22, Ghost Tail core): the grown region
    /// joins the document at the tail, the held line index is
    /// extended over the new length (its checkpoints and frontier
    /// carry over, so the background build resumes from the old
    /// frontier — see [`crate::line_index::LineIndex::extended_to`]
    /// and [`crate::line_index::LineIndexBuilder::resume_from`]),
    /// and the caret, selection, scroll position and mode are all
    /// left exactly as they were. `tail` supplies the grown bytes
    /// for the piece table's backing (see
    /// [`PieceTable::extend_original`]).
    ///
    /// Growth is only sound for a document that still is the file:
    /// any edit since open (including recorded-but-undone history)
    /// refuses with [`GrowthError::EditedDocument`], and a
    /// `new_len` that is not beyond the current length refuses
    /// with [`GrowthError::NotGrowth`] — shrinks and same-size
    /// changes stay with the external-change detection, which this
    /// method never replaces.
    pub fn note_file_growth(
        &mut self,
        new_len: u64,
        tail: &[u8],
    ) -> Result<GrowthReport, GrowthError> {
        let old_len = self.table.original_file_size;
        if new_len <= old_len {
            return Err(GrowthError::NotGrowth {
                current: old_len,
                requested: new_len,
            });
        }
        if self.dirty
            || self.history.can_undo()
            || self.history.can_redo()
            || self.index_dirty_from.is_some()
        {
            return Err(GrowthError::EditedDocument);
        }
        self.grow_with(new_len, |table| table.extend_original(new_len, tail))
    }

    /// Ghost Tail adoption with a re-mapped backing (roadmap item
    /// 22, window path): the same guards as [`Editor::note_file_growth`],
    /// but the grown original is adopted from `backing` — the shell
    /// re-opens the grown file, maps it fresh, and hands the new
    /// mapping in, because a memory map cannot grow in place. The
    /// caller must pass a backing whose length is exactly `new_len`
    /// (the piece table refuses anything else).
    pub fn adopt_grown_backing(
        &mut self,
        new_len: u64,
        backing: crate::piece_table::OriginalBuffer,
    ) -> Result<GrowthReport, GrowthError> {
        let old_len = self.table.original_file_size;
        if new_len <= old_len {
            return Err(GrowthError::NotGrowth {
                current: old_len,
                requested: new_len,
            });
        }
        if self.dirty
            || self.history.can_undo()
            || self.history.can_redo()
            || self.index_dirty_from.is_some()
        {
            return Err(GrowthError::EditedDocument);
        }
        self.grow_with(new_len, |table| {
            table.adopt_grown_original(new_len, backing)
        })
    }

    /// The shared tail of both growth paths: apply the piece-table
    /// growth, then extend the held line index's promise to the new
    /// length (an index describing anything else is dropped and
    /// rebuilds lazily instead of lying).
    fn grow_with(
        &mut self,
        new_len: u64,
        apply: impl FnOnce(
            &mut PieceTable,
        ) -> Result<
            crate::piece_table::Growth,
            crate::piece_table::ExtendOriginalError,
        >,
    ) -> Result<GrowthReport, GrowthError> {
        let old_len = self.table.original_file_size;
        let growth = apply(&mut self.table).map_err(GrowthError::Backing)?;
        // The held index was built over the old length: extend its
        // promise to the new length so it stays the valid partial
        // index of the grown document. An index describing anything
        // else (which an unedited document cannot hold) is dropped
        // and will rebuild lazily instead of lying.
        match &self.index {
            Some(index) if index.total_length() == old_len => {
                self.index = index.extended_to(new_len);
            }
            Some(_) => self.index = None,
            None => {}
        }
        self.walk_cache = None;
        self.row_anchor = None;
        Ok(GrowthReport {
            old_len,
            new_len,
            added: growth.added,
            index_frontier: self.index_frontier(),
        })
    }

    /// Number of lines in the document (0 for an empty document).
    /// Exact when the index is current, and exact after edits whose
    /// newline effect is tracked (typing, deletion, replace); after
    /// undo/redo it falls back to the index's own count until a
    /// re-index — a documented approximation only the scrollbar and
    /// page clamping consume. While the staged index is still
    /// partial it returns the indexed prefix's line count — a lower
    /// bound for the document, never a claimed total; the shell
    /// shows the build's percentage instead of presenting this as
    /// the document's line count.
    pub fn line_count(&mut self) -> u64 {
        self.ensure_index();
        let index = self.index.as_ref().expect("index present");
        if !index.is_complete() {
            return index.line_count();
        }
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
            let partial = !index.is_complete();
            if partial {
                // Partial (staged) index: the row anchor memoizes
                // the last line start resolved this way. Paint
                // passes ask for the first visible line every frame
                // while the walk cache sits a page ahead of it, so
                // without the anchor a view parked past the
                // frontier would re-scan the whole unindexed gap
                // on every frame.
                if let Some((_, start)) = self.row_anchor.filter(|(l, _)| *l == line) {
                    return Some(start);
                }
                // A cached walk position at or before the target
                // is the next cheapest exact route — sequential
                // row fetches stay proportional to the page instead
                // of re-scanning the unindexed gap from the last
                // checkpoint for every row.
                if let Some((wl, ws)) = self.walk_cache.filter(|(l, _)| *l <= line) {
                    let resolved = self.walk_lines((wl, ws), line);
                    if let Some(start) = resolved {
                        self.row_anchor = Some((line, start));
                    }
                    return resolved;
                }
            }
            let table = &self.table;
            // A partial index resolves through the forward
            // resolver: lines past the frontier have no checkpoints
            // yet, but their starts are still exact — the scan walks
            // from the last checkpoint, counting blocks at speed.
            let start = if partial {
                index.line_start_offset_forward(line, |offset, out| table.read_into(offset, out))
            } else {
                index.line_start_offset(line, |offset, out| table.read_into(offset, out))
            };
            if let Some(start) = start {
                self.walk_cache = Some((line, start));
                if partial {
                    self.row_anchor = Some((line, start));
                }
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
        let (cur_line, cur_start) = match self.walk_cache {
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
        self.walk_lines((cur_line, cur_start), line)
    }

    /// Walk forward from a known `(line, raw start)` to `line`, one
    /// line at a time, refreshing the walk cache as it goes. The
    /// cost is the bytes of the lines crossed. `None` when no
    /// further line exists.
    fn walk_lines(&mut self, from: (u64, u64), line: u64) -> Option<u64> {
        let (mut cur_line, mut cur_start) = from;
        while cur_line < line {
            let (_, _, next) = self.line_bounds(cur_start);
            // `next` at the end of the document is the
            // no-further-line sentinel (with or without a trailing
            // newline): the line that would start there is the
            // phantom after the last line and does not exist, the
            // same answer the index resolvers give.
            if next <= cur_start || next >= self.total_len() {
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
        // The count gate applies only to a complete index: while
        // the staged build is partial, the prefix count would
        // wrongly reject lines past the frontier — the resolver
        // below answers for them exactly (or returns None past the
        // last line).
        let complete = self.index.as_ref().is_some_and(|i| i.is_complete());
        if self.index_dirty_from.is_none() && complete && line >= self.line_count() {
            return None;
        }
        let raw_start = self.line_start(line)?;
        let (start, end, _) = self.line_bounds(raw_start);
        Some((start, end))
    }

    /// Byte offset where a line's content starts (after any
    /// byte-order mark on line 0), or `None` when the line does not
    /// exist. The shell's size-proportional scrollbar uses this
    /// while the staged index is partial, when line numbers do not
    /// yet span the document but byte positions always do.
    pub fn line_start_byte(&mut self, line: u64) -> Option<u64> {
        self.line_content_range(line).map(|(start, _)| start)
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
        // Forward scan from the seed to `offset`, counting
        // newlines a 64 KB block at a time (SWAR) so a caret deep
        // past the indexed frontier — or deep past an edit — costs
        // a scan-speed pass, not a byte-at-a-time walk. Only the
        // final block (the one containing `offset`) and any short
        // read are scanned byte-wise, where each newline's exact
        // position is tracked as before.
        let mut buffer = vec![0u8; 65536];
        while pos < offset {
            let want = ((offset - pos).min(buffer.len() as u64)) as usize;
            let read = self.table.read_into(pos, &mut buffer[..want]);
            if read == 0 {
                break;
            }
            let chunk = &buffer[..read];
            if read == buffer.len() {
                // A whole block (which by the `want` cap ends at or
                // before `offset`): count it in bulk; the line
                // containing `offset` starts after its last newline,
                // if it has one.
                let in_block = count_newlines_in(chunk);
                if in_block > 0 {
                    line += in_block;
                    let last = chunk
                        .iter()
                        .rposition(|&b| b == b'\n')
                        .expect("counted newlines are present");
                    line_start = Some(pos + last as u64 + 1);
                }
            } else {
                for (i, &b) in chunk.iter().enumerate() {
                    if b == b'\n' {
                        line += 1;
                        line_start = Some(pos + i as u64 + 1);
                    }
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
        // While the staged index is partial there is no document
        // total to bound against: rows are fetched until the
        // resolver reports no line, which is exact at the end of
        // the document and never blocks inside the prefix.
        self.ensure_index();
        let total = if self.index_is_partial() {
            u64::MAX
        } else {
            self.line_count()
        };
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
    /// Row width, grouping, and group byte order follow the current
    /// hex layout; the ASCII column always reads in file order.
    pub fn visible_hex_rows(&self, first_row: u64, count: usize) -> Vec<HexRow> {
        let width = self.hex_width();
        let total_rows = hexview::row_count_w(self.total_len(), width);
        let mut rows = Vec::new();
        let mut row = first_row;
        while rows.len() < count && row < total_rows {
            let address = hexview::row_offset_w(row, width);
            let len = width.min(self.total_len() - address);
            let bytes = self.table.read(address, len);
            let hex = hexview::format_hex(&bytes, &self.hex_layout);
            let mut ascii = String::new();
            for &b in bytes.iter() {
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
                let step = self.hex_width() as i64 * delta;
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
                let row = hexview::row_for_offset_w(self.caret, self.hex_width());
                self.set_caret(hexview::row_offset_w(row, self.hex_width()), extend);
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
                let width = self.hex_width();
                let row = hexview::row_for_offset_w(self.caret, width);
                let end = hexview::row_offset_w(row, width)
                    .saturating_add(width)
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
                let row = hexview::row_for_offset_w(self.caret, self.hex_width());
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
        self.row_anchor = None;
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

    /// Insert `count` zero bytes at the caret (roadmap R4 zero-run
    /// insert): one undo entry, and no bytes are stored however
    /// large `count` is — the run lives in the piece table as a
    /// single [`BufferSource::Zeroes`](crate::piece_table::BufferSource::Zeroes)
    /// piece until a save streams it out. With a selection, the
    /// selection is replaced by the zero run (still one undo
    /// entry).
    pub fn insert_zeroes(&mut self, count: u64) -> Result<(), EditorError> {
        self.require_writable()?;
        if count == 0 {
            return Ok(());
        }
        if let Some((start, end)) = self.selection_range() {
            let removed = self.table.read(start, end - start);
            // Zero bytes contain no '\n', so only the removed
            // content moves the newline delta.
            self.note_newline_delta(&[], &removed);
            self.history
                .do_segment_replace(&mut self.table, start, end, |table| {
                    let previous = table.delete(start, end - start);
                    table.insert_zeroes(start, count);
                    crate::undo::EditAction::Replace {
                        offset: start,
                        previous_pieces: previous,
                        inserted_length: count,
                        replacements: 1,
                    }
                });
            self.caret = start + count;
            self.anchor = None;
            self.after_mutation(start);
        } else {
            let at = self.caret;
            self.history.do_insert_zeroes(&mut self.table, at, count);
            self.caret += count;
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

    /// Replace the single byte at `at` in place, as one undoable
    /// edit, WITHOUT moving the caret (the hex bit tooling flips bits
    /// of the byte under the caret through this). Replacing with the
    /// byte already there, or at/past the end of the document, is a
    /// no-op that records no history.
    pub fn replace_byte_at(&mut self, at: u64, byte: u8) -> Result<(), EditorError> {
        self.require_writable()?;
        if at >= self.total_len() {
            return Ok(());
        }
        let old = self.table.byte_at(at).unwrap_or(0);
        if old == byte {
            return Ok(());
        }
        self.note_newline_delta(&[byte], &[old]);
        search::replace_one(&mut self.table, &mut self.history, at, 1, &[byte]);
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
        self.row_anchor = None;
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
        self.row_anchor = None;
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

    /// Save over the document's own file with explicit save options
    /// (roadmap items 31/32: the line-ending and BOM transforms the
    /// window's File ▸ Save Options menu chooses). Same pipeline as
    /// [`Editor::save`], but routed through
    /// `PieceTable::save_to_with_options`.
    pub fn save_with_options(
        &mut self,
        options: &crate::save::SaveOptions,
    ) -> Result<SaveReport, EditorError> {
        self.require_writable_save()?;
        let path = self.path.clone().ok_or(EditorError::NoPath)?;
        let report = self
            .table
            .save_to_with_options(&path, &mut self.history, options)?;
        Ok(self.finish_save(report))
    }

    /// Save As with explicit save options; the path is adopted only
    /// when the save succeeds, like [`Editor::save_as`].
    pub fn save_as_with_options(
        &mut self,
        path: &Path,
        options: &crate::save::SaveOptions,
    ) -> Result<SaveReport, EditorError> {
        let report = self
            .table
            .save_to_with_options(path, &mut self.history, options)?;
        self.path = Some(path.to_path_buf());
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
            self.row_anchor = None;
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
    fn hex_layout_changes_row_math_and_formatting() {
        let data: Vec<u8> = (0..40).collect();
        let mut e = ed(&data);
        e.set_mode(EditorMode::Hex);
        // Narrower rows: more of them, addresses in steps of 8.
        e.set_hex_layout(hexview::HexLayout {
            bytes_per_row: 8,
            group: 1,
            big_endian: false,
        });
        let rows = e.visible_hex_rows(0, 10);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[1].address, 8);
        assert_eq!(rows[0].hex, "00 01 02 03  04 05 06 07");
        assert_eq!(rows[0].ascii, "........");
        // Vertical movement steps one narrow row at a time.
        e.move_down(false);
        assert_eq!(e.caret(), 8);
        e.move_end(false);
        assert_eq!(e.caret(), 16, "end of the second 8-byte row");
        // Home goes to the start of the caret's own row.
        e.move_caret_to(11, false);
        e.move_home(false);
        assert_eq!(e.caret(), 8);
        // Wider rows: two rows cover the file.
        e.set_hex_layout(hexview::HexLayout {
            bytes_per_row: 32,
            group: 1,
            big_endian: false,
        });
        let rows = e.visible_hex_rows(0, 10);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].address, 32);
        assert_eq!(rows[1].len, 8);
        e.move_doc_start(false);
        e.move_down(false);
        assert_eq!(e.caret(), 32);
        // Grouping changes the display only: the caret and the bytes
        // stay byte-based.
        e.set_hex_layout(hexview::HexLayout {
            bytes_per_row: 16,
            group: 4,
            big_endian: true,
        });
        let rows = e.visible_hex_rows(0, 1);
        assert!(rows[0].hex.starts_with("03 02 01 00"), "{}", rows[0].hex);
        assert_eq!(rows[0].ascii, "................");
        e.move_doc_start(false);
        e.overwrite_byte(0xEE).unwrap();
        assert_eq!(content(&e)[0], 0xEE);
    }

    #[test]
    fn hex_layout_keeps_the_caret_row_in_view() {
        let data: Vec<u8> = (0..200).collect();
        let mut e = ed(&data);
        e.set_mode(EditorMode::Hex);
        e.move_caret_to(150, false);
        e.set_first_row(9); // row of the caret at width 16
        assert_eq!(e.first_row(), 9);
        // At width 8 the caret sits on row 18; the scroll row must
        // not leave it above the viewport.
        e.set_hex_layout(hexview::HexLayout {
            bytes_per_row: 8,
            group: 1,
            big_endian: false,
        });
        assert_eq!(e.first_row(), 9);
        e.set_first_row(24); // last row at width 8 (25 rows)
        e.set_hex_layout(hexview::HexLayout {
            bytes_per_row: 64,
            group: 1,
            big_endian: false,
        });
        // Only 4 rows exist now; the scroll row clamps, then the
        // caret's row (2) pulls it up.
        assert_eq!(e.first_row(), 2);
    }

    #[test]
    fn save_with_options_applies_the_chosen_transform() {
        let path = temp_path("saveopts-doc.txt");
        std::fs::write(&path, b"a\nb\n").unwrap();
        let mut e = Editor::open_from_path(&path, false).unwrap();
        let options = crate::save::SaveOptions {
            eol: crate::save::EolChoice::Crlf,
            bom: crate::save::BomChoice::Add,
            ..crate::save::SaveOptions::default()
        };
        e.save_with_options(&options).unwrap();
        assert!(!e.is_dirty());
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk, b"\xEF\xBB\xBFa\r\nb\r\n");
        // Save As with options adopts the new path and transforms too.
        let other = temp_path("saveopts-copy.txt");
        e.save_as_with_options(
            &other,
            &crate::save::SaveOptions {
                eol: crate::save::EolChoice::Lf,
                bom: crate::save::BomChoice::Remove,
                ..crate::save::SaveOptions::default()
            },
        )
        .unwrap();
        assert_eq!(e.path(), Some(other.as_path()));
        assert_eq!(std::fs::read(&other).unwrap(), b"a\nb\n");
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&other).ok();
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

    /// A synthetic multi-MB document for the staged-open tests:
    /// `lines` lines of 32 bytes each ("line NNNNNN" + padding +
    /// newline) — 200,000 lines is 6.4 MB, past the 4 MB head
    /// region, with every line start at `line * 32` by construction.
    fn big_doc(lines: u64) -> Vec<u8> {
        let mut data = Vec::new();
        for i in 0..lines {
            let mut line = format!("line {i:06}").into_bytes();
            line.resize(31, b'.');
            line.push(b'\n');
            data.extend_from_slice(&line);
        }
        data
    }

    fn big_line_text(i: u64) -> String {
        let mut line = format!("line {i:06}");
        line.push_str(&".".repeat(20));
        line
    }

    #[test]
    fn staged_open_indexes_only_the_head_and_stays_exact() {
        let data = big_doc(200_000);
        let total = data.len() as u64;
        let mut e = Editor::open_from_bytes(data.clone(), None, false);
        // Nothing is indexed until the first line question, and
        // that question pays only for the head region.
        assert_eq!(e.index_frontier(), 0);
        let prefix_count = e.line_count();
        assert!(e.index_is_partial(), "open must not index the whole file");
        assert!(!e.has_complete_index());
        let frontier = e.index_frontier();
        assert!(frontier >= INDEX_HEAD_BYTES && frontier < total);
        // The reported count is the indexed prefix's own count
        // (32-byte lines: the frontier is a whole number of lines).
        assert_eq!(prefix_count, frontier / 32);
        assert!(prefix_count < 200_000, "not a claimed document total");
        // The first screen paints from the head region.
        let rows = e.visible_text_rows(0, 40);
        assert_eq!(rows.len(), 40);
        assert_eq!(rows[0].text, big_line_text(0));
        assert_eq!(rows[39].text, big_line_text(39));
        // Lines past the frontier resolve exactly by forward scan.
        assert_eq!(e.line_start_byte(150_000), Some(150_000 * 32));
        // The resolution is memoized in the row anchor, so a paint
        // pass asking for the same first row again is O(1) instead
        // of a fresh scan across the unindexed gap.
        assert_eq!(e.row_anchor, Some((150_000, 150_000 * 32)));
        assert_eq!(e.line_start_byte(150_000), Some(150_000 * 32));
        assert_eq!(e.line_text(199_999).unwrap(), big_line_text(199_999));
        assert_eq!(e.line_text(200_000), None);
        // The caret at the very end resolves exactly, and agrees
        // with a fully indexed reference editor.
        let mut reference = Editor::open_from_bytes(data.clone(), None, false);
        reference.rebuild_index();
        assert_eq!(e.line_col_of(total), reference.line_col_of(total));
        assert_eq!(e.line_col_of(3_333_333), reference.line_col_of(3_333_333));
    }

    #[test]
    fn staged_completion_attaches_and_matches_a_full_build() {
        let data = big_doc(200_000);
        let total = data.len() as u64;
        let mut e = Editor::open_from_bytes(data.clone(), None, false);
        e.seed_line_index();
        assert!(e.index_is_partial());
        // The background build's final snapshot attaches: the
        // editor becomes complete with the document's true totals.
        e.attach_line_index(LineIndex::build_from_bytes(&data));
        assert!(e.has_complete_index());
        assert!(!e.index_is_partial());
        assert_eq!(e.index_frontier(), total);
        assert_eq!(e.line_count(), 200_000);
        // A late, smaller snapshot never shrinks the frontier.
        e.attach_line_index(LineIndex::build_prefix(
            total,
            INDEX_HEAD_BYTES,
            |offset, out| {
                let start = offset as usize;
                if start >= data.len() {
                    return 0;
                }
                let n = out.len().min(data.len() - start);
                out[..n].copy_from_slice(&data[start..start + n]);
                n
            },
        ));
        assert_eq!(e.index_frontier(), total);
        assert_eq!(e.line_count(), 200_000);
    }

    #[test]
    fn edit_during_staged_build_discards_it_and_stays_exact() {
        let data = big_doc(200_000);
        let mut e = Editor::open_from_bytes(data.clone(), None, false);
        e.seed_line_index();
        assert!(e.index_is_partial());
        // An edit lands mid-document while the staged build is in
        // flight (inside the indexed prefix, at line 100,000).
        e.goto_line(100_001);
        assert_eq!(e.caret(), 100_000 * 32);
        e.insert_text("inserted line\n").unwrap();
        // The in-flight build describes the pre-edit bytes: its
        // final snapshot must be refused, exactly as the shell's
        // generation rule discards it.
        e.attach_line_index(LineIndex::build_from_bytes(&data));
        assert!(!e.has_complete_index(), "stale build must not attach");
        // The edited document, built from scratch as a reference.
        let mut edited = data.clone();
        edited.splice(3_200_000..3_200_000, b"inserted line\n".iter().copied());
        let mut reference = Editor::open_from_bytes(edited.clone(), None, false);
        reference.rebuild_index();
        assert_eq!(reference.line_count(), 200_001);
        // Positions and contents stay exact against the reference —
        // before the edit, at it, and at the end of the file.
        assert_eq!(e.line_text(100_000).unwrap(), "inserted line");
        assert_eq!(e.line_text(100_001).unwrap(), big_line_text(100_000));
        assert_eq!(e.line_text(200_000).unwrap(), big_line_text(199_999));
        for offset in [0u64, 3_200_000, 3_200_014, 3_200_025, edited.len() as u64] {
            assert_eq!(
                e.line_col_of(offset),
                reference.line_col_of(offset),
                "offset {offset}"
            );
        }
        // The count is the documented lower bound while partial and
        // stale — never the pre-edit total presented as current.
        assert!(e.line_count() <= reference.line_count());
        // The explicit rebuild (the baseline the shell returns to
        // at save/open) restores a complete, exact index of the
        // edited bytes.
        e.rebuild_index();
        assert!(e.has_complete_index());
        assert_eq!(e.line_count(), reference.line_count());
        assert_eq!(e.line_text(100_000).unwrap(), "inserted line");
        assert_eq!(e.line_text(200_001), None);
        assert_eq!(reference.line_text(200_001), None);
    }

    /// Measured on this VM, not asserted: open-return timings over
    /// a ~1 GB in-memory document. "Before" is the old open path —
    /// the first line question paying for a full synchronous index
    /// build (what `rebuild_index` still does on demand); "after"
    /// is the staged open, where the first line question pays only
    /// for the head region. Run with
    /// `cargo test --release bench_editor_open -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_editor_open_1gb() {
        // ~1 GB of 100-byte log-like lines, built by repeating a
        // 1 MB block (generation speed is not what is measured).
        let mut block = Vec::with_capacity(1_000_000);
        let mut n = 0u64;
        while block.len() < 1_000_000 {
            let mut line = format!("2026-11-01T00:00:00Z INFO worker request {n} ok").into_bytes();
            line.resize(99, b' ');
            line.push(b'\n');
            block.extend_from_slice(&line);
            n += 1;
        }
        let mut data = Vec::with_capacity(1_000_000_000 + block.len());
        while data.len() < 1_000_000_000 {
            data.extend_from_slice(&block);
        }
        let total = data.len() as u64;
        eprintln!("bench document: {total} bytes");

        let t = std::time::Instant::now();
        let mut e = Editor::open_from_bytes(data, None, false);
        let count = e.line_count();
        let after_ms = t.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "staged open, first line question: {after_ms:.2} ms \
             (prefix count {count}, frontier {})",
            e.index_frontier()
        );
        assert!(e.index_is_partial());

        let t = std::time::Instant::now();
        e.rebuild_index();
        let before_ms = t.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "full synchronous build (old open): {before_ms:.1} ms \
             ({:.0} MB/s)",
            total as f64 / (before_ms / 1000.0) / 1e6
        );
        assert!(e.has_complete_index());
        assert_eq!(e.line_count(), total / 100);
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

    // ---- Zero-run insert (roadmap R4) ------------------------------

    #[test]
    fn editor_insert_zeroes_and_undo() {
        let mut e = ed(b"ab");
        e.move_doc_end(false);
        e.insert_zeroes(3).unwrap();
        assert_eq!(content(&e), vec![b'a', b'b', 0, 0, 0]);
        assert_eq!(e.caret(), 5);
        assert!(e.is_dirty());
        // One undo entry removes the whole run.
        assert!(e.undo());
        assert_eq!(content(&e), b"ab");
        assert!(!e.is_dirty());
        assert!(e.redo());
        assert_eq!(content(&e), vec![b'a', b'b', 0, 0, 0]);
        // A huge run is one piece and stores nothing.
        let mut e = ed(b"");
        e.insert_zeroes(5_000_000).unwrap();
        assert_eq!(e.total_len(), 5_000_000);
        assert_eq!(e.table().piece_count(), 1);
        assert!(e.table().append_buffer.is_empty());
    }

    #[test]
    fn editor_insert_zeroes_replaces_selection_as_one_entry() {
        let mut e = ed(b"hello world");
        e.goto_line(1);
        e.move_caret_to(0, false);
        e.move_caret_to(5, true); // select "hello"
        assert_eq!(e.selection_range(), Some((0, 5)));
        e.insert_zeroes(2).unwrap();
        assert_eq!(content(&e), vec![0, 0, b' ', b'w', b'o', b'r', b'l', b'd']);
        assert_eq!(e.caret(), 2);
        assert!(e.undo());
        assert_eq!(content(&e), b"hello world");
        assert!(e.redo());
        assert_eq!(content(&e), vec![0, 0, b' ', b'w', b'o', b'r', b'l', b'd']);
    }

    // ---- Ghost Tail growth (roadmap item 22) ------------------------

    #[test]
    fn growth_appends_the_tail_and_keeps_position() {
        let mut e = ed(b"line one\nline two\n");
        e.seed_line_index();
        assert!(e.has_complete_index());
        e.move_caret_to(4, false);
        let report = e.note_file_growth(29, b"line three\n").unwrap();
        assert_eq!(
            report,
            GrowthReport {
                old_len: 18,
                new_len: 29,
                added: 11,
                index_frontier: 18,
            }
        );
        assert_eq!(content(&e), b"line one\nline two\nline three\n");
        assert_eq!(e.total_len(), 29);
        // The caret did not move and the document is not dirty:
        // growth is the file's own change, not an edit.
        assert_eq!(e.caret(), 4);
        assert!(!e.is_dirty());
        assert!(!e.can_undo());
        // The index is now the partial index of the grown source,
        // holding the old frontier; line questions stay exact.
        assert!(e.index_is_partial());
        assert_eq!(e.index_frontier(), 18);
        assert_eq!(e.line_text(2).unwrap(), "line three");
    }

    #[test]
    fn growth_resumes_and_completes_the_staged_index() {
        // Bigger than the head region so the open index is partial:
        // growth must extend that partial index, and a resumed
        // builder must finish it identically to a full build.
        let mut data = Vec::new();
        for i in 0..500_000u32 {
            data.extend_from_slice(format!("row {i}\n").as_bytes());
        }
        let old_len = data.len() as u64;
        let mut e = Editor::open_from_bytes(data.clone(), None, false);
        e.seed_line_index();
        assert!(e.index_is_partial());
        let frontier_before = e.index_frontier();
        // Grow by exactly one more line.
        data.extend_from_slice(b"row 500000\n");
        let report = e
            .note_file_growth(data.len() as u64, b"row 500000\n")
            .unwrap();
        assert_eq!(report.old_len, old_len);
        assert_eq!(report.index_frontier, frontier_before);
        assert!(e.index_is_partial());
        assert_eq!(e.line_text(500_000).unwrap(), "row 500000");
    }

    #[test]
    fn growth_refuses_shrinks_and_edited_documents() {
        // Shrink and same-size are not growth.
        let mut e = ed(b"abc\n");
        assert_eq!(
            e.note_file_growth(4, b""),
            Err(GrowthError::NotGrowth {
                current: 4,
                requested: 4
            })
        );
        assert_eq!(
            e.note_file_growth(2, b""),
            Err(GrowthError::NotGrowth {
                current: 4,
                requested: 2
            })
        );
        // An edited document refuses (and stays untouched).
        let mut e = ed(b"abc\n");
        e.move_doc_end(false);
        e.insert_text("x").unwrap();
        assert_eq!(
            e.note_file_growth(9, b"more\n"),
            Err(GrowthError::EditedDocument)
        );
        assert_eq!(content(&e), b"abc\nx");
        // Even an edit that was undone leaves history behind, and
        // history is enough to refuse.
        assert!(e.undo());
        assert_eq!(
            e.note_file_growth(9, b"more\n"),
            Err(GrowthError::EditedDocument)
        );
        assert_eq!(content(&e), b"abc\n");
    }

    #[test]
    fn growth_adoption_with_a_fresh_backing_matches_note_file_growth() {
        // The window path: re-map the grown file and adopt the new
        // backing. Same guards, same report, same content.
        use crate::piece_table::OriginalBuffer;
        let mut e = ed(b"line one\nline two\n");
        e.seed_line_index();
        e.move_caret_to(4, false);
        let grown = b"line one\nline two\nline three\n".to_vec();
        let report = e
            .adopt_grown_backing(29, OriginalBuffer::Memory(grown))
            .unwrap();
        assert_eq!(report.old_len, 18);
        assert_eq!(report.new_len, 29);
        assert_eq!(report.added, 11);
        assert_eq!(content(&e), b"line one\nline two\nline three\n");
        assert_eq!(e.caret(), 4);
        assert!(!e.is_dirty());
        assert!(e.index_is_partial());
        assert_eq!(e.line_text(2).unwrap(), "line three");
        // A backing whose length disagrees with the claimed growth
        // is refused by the piece table, surfaced as a backing error.
        let mut e = ed(b"abc\n");
        assert!(matches!(
            e.adopt_grown_backing(9, OriginalBuffer::Memory(b"abc\n".to_vec())),
            Err(GrowthError::Backing(_))
        ));
    }

    #[test]
    fn replace_byte_at_edits_in_place_without_moving_the_caret() {
        let mut e = ed(b"\x00\x41\n");
        e.move_caret_to(1, false);
        e.replace_byte_at(0, 0x80).unwrap();
        assert_eq!(content(&e), vec![0x80, 0x41, b'\n']);
        assert_eq!(e.caret(), 1);
        assert!(e.is_dirty());
        // Same byte again: no change, no extra history entry.
        e.replace_byte_at(0, 0x80).unwrap();
        assert!(e.undo());
        assert_eq!(content(&e), vec![0x00, 0x41, b'\n']);
        assert!(!e.can_undo());
        // Past the end is a no-op.
        e.replace_byte_at(99, 0xFF).unwrap();
        assert_eq!(content(&e), vec![0x00, 0x41, b'\n']);
    }

    #[test]
    fn filter_view_over_the_editor_index() {
        use crate::filter::FilterQuery;
        use crate::search::SearchOptions;
        let mut e = ed(b"alpha\nbeta needle\ngamma\nneedle two\n");
        e.seed_line_index();
        let view = e
            .filter_view(FilterQuery::literal(b"needle", SearchOptions::default()))
            .unwrap();
        assert_eq!(view.len(), 2);
        assert_eq!(view.line_at_row(0).unwrap().line_no, 1);
        assert_eq!(view.line_at_row(1).unwrap().line_no, 3);
        assert!(view.is_complete());
        // The index snapshot is available for a resumed build and is
        // withheld once edits make it stale.
        assert!(e.line_index_snapshot().is_some());
        e.move_doc_end(false);
        e.insert_text("x").unwrap();
        assert!(e.line_index_snapshot().is_none());
    }
}
