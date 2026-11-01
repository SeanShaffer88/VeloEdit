//! VeloEdit core library.
//!
//! Everything the program can do is an operation in this library first;
//! the GUI shell and the command line both call the same operations, so no
//! capability exists only inside a window or only inside the CLI
//! (see `docs/PARITY.md` for the operation registry).
//!
//! The core contains the platform-independent operations only: the piece
//! table (SPEC.md section 2), the sparse line index and hex arithmetic
//! (section 3), undo/redo (section 5.2), the headless binary diff
//! (section 7), command-line parsing, and — owner-confirmed v1 features
//! the blueprint omits — text encodings ([`encoding`]) and streaming
//! search/replace over the piece table ([`search`]). The Win32 /
//! Direct2D shell and the Windows memory-mapping and atomic-save
//! operations arrive in later slices; the seams for them are documented
//! in each module.
//!
//! No `unsafe` code exists in this slice: the only `unsafe` the design
//! needs is the Windows FFI in later slices, which will be scoped to the
//! smallest possible modules there.

#![forbid(unsafe_code)]

pub mod cli;
pub mod diff;
pub mod encoding;
pub mod hexview;
pub mod line_index;
pub mod piece_table;
pub mod search;
pub mod undo;

use piece_table::{PieceNode, PieceTable};
use undo::UndoStack;

/// Operation: compare two byte sources (SPEC.md section 7, `--diff`).
pub fn op_diff(a: &[u8], b: &[u8]) -> diff::DiffReport {
    diff::diff_bytes(a, b)
}

/// Operation: read a slice of the document (viewport reads, exports).
pub fn op_read(table: &PieceTable, offset: u64, len: u64) -> Vec<u8> {
    table.read(offset, len)
}

/// Operation: insert bytes into the document, recorded for undo.
pub fn op_insert(table: &mut PieceTable, history: &mut UndoStack, offset: u64, bytes: &[u8]) {
    history.do_insert(table, offset, bytes);
}

/// Operation: delete bytes from the document, recorded for undo. Returns
/// the removed content pieces.
pub fn op_delete(
    table: &mut PieceTable,
    history: &mut UndoStack,
    offset: u64,
    len: u64,
) -> Vec<PieceNode> {
    history.do_delete(table, offset, len)
}

/// Operation: undo the most recent edit. Returns false when there is
/// nothing to undo.
pub fn op_undo(table: &mut PieceTable, history: &mut UndoStack) -> bool {
    history.apply_undo(table)
}

/// Operation: redo the most recently undone edit. Returns false when there
/// is nothing to redo.
pub fn op_redo(table: &mut PieceTable, history: &mut UndoStack) -> bool {
    history.apply_redo(table)
}

/// Operation: locate the byte offset where a 0-based line starts, using a
/// sparse line index (SPEC.md section 3.1). `read_at` follows the contract
/// documented on [`line_index::LineIndex::build`].
pub fn op_line_locate(
    index: &line_index::LineIndex,
    line_no: u64,
    read_at: impl FnMut(u64, &mut [u8]) -> usize,
) -> Option<u64> {
    index.line_start_offset(line_no, read_at)
}

/// Operation: hex-mode row to byte offset (SPEC.md section 3.1).
pub fn op_hex_offset(row: u64) -> u64 {
    hexview::row_offset(row)
}

/// Operation: detect the text encoding of a byte sample (owner-confirmed
/// v1 feature). See [`encoding::detect`] for the rules.
pub fn op_detect_encoding(sample: &[u8]) -> encoding::Detection {
    encoding::detect(sample)
}

/// Operation: find every occurrence of `needle` (already encoded in the
/// document's encoding) in the document, streaming over the piece table
/// without materialising it. Overlapping occurrences are reported.
pub fn op_find_all(table: &PieceTable, needle: &[u8], options: &search::SearchOptions) -> Vec<u64> {
    search::find_all(table, needle, options)
}

/// Operation: find the first occurrence of `needle` at or after
/// `from_offset`.
pub fn op_find_next(
    table: &PieceTable,
    needle: &[u8],
    from_offset: u64,
    options: &search::SearchOptions,
) -> Option<u64> {
    search::find_next(table, needle, from_offset, options)
}

/// Operation: replace the `needle_len` bytes at `offset` with
/// `replacement`, recorded as a single undoable history entry. Returns
/// the recorded action, or `None` when the request was a no-op.
pub fn op_replace_one(
    table: &mut PieceTable,
    history: &mut UndoStack,
    offset: u64,
    needle_len: u64,
    replacement: &[u8],
) -> Option<undo::EditAction> {
    search::replace_one(table, history, offset, needle_len, replacement)
}

/// Operation: replace every non-overlapping occurrence of `needle` with
/// `replacement`, as a single undoable history entry (one undo restores
/// the document exactly).
pub fn op_replace_all(
    table: &mut PieceTable,
    history: &mut UndoStack,
    needle: &[u8],
    replacement: &[u8],
    options: &search::SearchOptions,
) -> search::ReplaceReport {
    search::replace_all(table, history, needle, replacement, options)
}
