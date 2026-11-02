//! VeloEdit core library.
//!
//! Everything the program can do is an operation in this library first;
//! the GUI shell and the command line both call the same operations, so no
//! capability exists only inside a window or only inside the CLI
//! (see `docs/PARITY.md` for the operation registry).
//!
//! The core contains the platform-independent operations: the piece
//! table (SPEC.md section 2), the sparse line index and hex arithmetic
//! (section 3), undo/redo (section 5.2), the atomic save pipeline
//! (section 5.1, [`save`]), the headless binary diff (section 7),
//! command-line parsing, and — owner-confirmed v1 features the
//! blueprint omits — text encodings ([`encoding`]) and streaming
//! search/replace over the piece table ([`search`]). Slice 4 adds the
//! GUI-independent editor state machine ([`editor`]), the built-in
//! theme library ([`theme`]), portable settings ([`settings`]) and the
//! Win32 / Direct2D shell ([`gui_win`], Windows only). On Windows, the
//! read-only memory-mapped file backing lives in [`mmap_win`]. The
//! roadmap R3 cores add hand-written streaming hashes ([`hash`]), the
//! data inspector's byte readings ([`inspector`]), per-file bookmarks
//! ([`bookmarks`]) and delta-log crash recovery ([`recovery`]). The
//! roadmap R4 cores add the structural filter view ([`filter`]),
//! multi-range slicing ([`slicing`]), delta patch export ([`patch`]),
//! the strings scan ([`strings`]), the entropy map ([`entropy`]),
//! pointer targets and the jump history ([`pointers`]), log time
//! deltas ([`timedeltas`]) and the delimiter grid ([`delimited`]).
//! Slice R4c adds rebindable keys ([`keybindings`]) and the
//! block-level structural diff ([`structdiff`]). The roadmap R5a
//! cores add the streaming regex engine ([`regex`]), binary
//! templates ([`templates`]), the line diff ([`linediff`]), the
//! x86-64 disassembly length decoder ([`disasm`]), the
//! external-tools protocol ([`exttools`]) and multi-range /
//! rectangular editing on the undo stack ([`undo`]).
//!
//! Unsafe policy (PROJECT_RULES.md): the crate root is
//! `#![deny(unsafe_code)]`, and the only modules allowed to opt back in
//! with `#[allow(unsafe_code)]` are the Windows FFI modules that cannot
//! avoid it — [`mmap_win`], [`gui_win`] and the Windows half of [`save`]
//! (the `ReplaceFileW` / `FlushFileBuffers` calls). Every `unsafe` block
//! there carries a `// SAFETY:` comment. No other module may contain
//! `unsafe`.

#![deny(unsafe_code)]

pub mod bookmarks;
pub mod cli;
pub mod delimited;
pub mod diff;
pub mod disasm;
pub mod editor;
pub mod encoding;
pub mod entropy;
pub mod exttools;
pub mod filter;
#[cfg(windows)]
pub mod gui_win;
pub mod hash;
pub mod hexview;
pub mod inspector;
pub mod keybindings;
pub mod line_index;
pub mod linediff;
pub mod mapwindow;
#[cfg(windows)]
pub mod mmap_win;
pub mod panes;
pub mod patch;
pub mod piece_table;
pub mod pointers;
pub mod recovery;
pub mod regex;
pub mod save;
pub mod search;
pub mod settings;
pub mod slicing;
pub mod strings;
pub mod structdiff;
pub mod tabs;
pub mod templates;
pub mod theme;
pub mod timedeltas;
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

/// Operation: find every occurrence of `needle` lying fully inside
/// `[start, end)` (roadmap item 11, scoped search core).
pub fn op_find_in_range(
    table: &PieceTable,
    needle: &[u8],
    options: &search::SearchOptions,
    start: u64,
    end: u64,
) -> Vec<u64> {
    search::find_in_range(table, needle, options, start, end)
}

/// Operation: replace every non-overlapping occurrence of `needle`
/// lying fully inside `[start, end)`, as one undoable history entry
/// (roadmap item 11).
pub fn op_replace_all_in_range(
    table: &mut PieceTable,
    history: &mut UndoStack,
    needle: &[u8],
    replacement: &[u8],
    options: &search::SearchOptions,
    start: u64,
    end: u64,
) -> search::ReplaceReport {
    search::replace_all_in_range(table, history, needle, replacement, options, start, end)
}

/// Operation: find every occurrence of a masked byte pattern
/// (`"4A ?? FF"`, roadmap item 12), streaming over the piece table.
pub fn op_find_masked(
    table: &PieceTable,
    pattern: &search::MaskedPattern,
    max_results: Option<usize>,
) -> Vec<u64> {
    search::find_masked_all(table, pattern, max_results)
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

/// Operation: find the occurrences of `needle` starting inside the
/// window `[start, end)` (roadmap item 10) — the incremental form a
/// background worker drives window by window; concatenated over
/// consecutive windows it equals [`op_find_all`].
pub fn op_find_in_window(
    table: &PieceTable,
    needle: &[u8],
    options: &search::SearchOptions,
    start: u64,
    end: u64,
) -> Vec<u64> {
    search::find_in_window(table, needle, options, start, end)
}

/// Operation: read the bytes at a position as inspector values —
/// integers and floats in both byte orders, hex/binary renderings,
/// and Unix-timestamp readings (roadmap item 15).
pub fn op_inspect(bytes: &[u8]) -> inspector::InspectorReport {
    inspector::inspect(bytes)
}

/// Operation: hash a byte source with MD5, SHA-1, SHA-256, CRC32,
/// and xxHash64, streamed through fixed-size windows (roadmap
/// item 37). `read_at` follows the contract of
/// [`hash::hash_byte_source`].
pub fn op_hash_byte_source(
    total: u64,
    read_at: impl FnMut(u64, &mut [u8]) -> usize,
) -> hash::FileHashes {
    hash::hash_byte_source(total, read_at)
}

/// Operation: split a file into fixed-size parts, streamed (roadmap
/// item 36). Parts land in `out_dir` as `<name>.part001`, … and are
/// returned in order.
pub fn op_split_file(
    src: &std::path::Path,
    part_size: u64,
    out_dir: &std::path::Path,
) -> std::io::Result<Vec<std::path::PathBuf>> {
    save::split_file(src, part_size, out_dir)
}

/// Operation: save the document over its file atomically (SPEC.md
/// section 5.1) and adopt the saved file as the new original backing.
/// Saving is a history boundary: `history` is cleared on success.
pub fn op_save(
    table: &mut PieceTable,
    history: &mut UndoStack,
    target: &std::path::Path,
) -> Result<save::SaveReport, save::SaveError> {
    table.save_to(target, history)
}

/// Operation: save a copy of the document to a different path atomically
/// (SPEC.md section 5.1 steps 1–5; the table itself is not reset). See
/// [`save::save_atomic`] for the mapping caveat that applies when the
/// copy target is the mapped file itself — use [`op_save`] for that.
pub fn op_save_copy(
    table: &PieceTable,
    target: &std::path::Path,
) -> Result<save::SaveReport, save::SaveError> {
    save::save_atomic(table, target, &save::SaveOptions::default())
}

/// Operation: insert a zero run into the document, recorded for
/// undo (R4 core). The zeros are a piece-table region, never
/// materialised bytes; every read path (editor, find, save,
/// extract) yields `0x00` for them. See
/// [`piece_table::PieceTable::insert_zeroes`].
pub fn op_insert_zeroes(table: &mut PieceTable, history: &mut UndoStack, offset: u64, count: u64) {
    history.do_insert_zeroes(table, offset, count);
}

/// Operation: build the structural filter view (roadmap item 18,
/// R4 core): the lines containing a match, with two-way mapping
/// between filtered rows and full-file positions. See
/// [`filter::build_filter`] for the partial-index contract.
pub fn op_build_filter(
    table: &PieceTable,
    index: &line_index::LineIndex,
    query: filter::FilterQuery,
) -> filter::FilterView {
    filter::build_filter(table, index, query)
}

/// Operation: stream the byte/line ranges named by a slicing spec
/// out of the document, in order (R4 core, extending item 44). See
/// [`slicing`] for the spec grammar and its rules.
pub fn op_extract_ranges(
    table: &PieceTable,
    index: Option<&line_index::LineIndex>,
    spec: &str,
    out: &mut impl std::io::Write,
) -> Result<slicing::ExtractReport, slicing::SliceError> {
    slicing::extract_ranges(table, index, spec, out)
}

/// Operation: build a `VELOPAT1` delta patch from the original
/// byte source to the edited document (R4 core). See [`patch`].
pub fn op_build_patch(
    original_len: u64,
    read_original: impl FnMut(u64, &mut [u8]) -> usize,
    table: &PieceTable,
) -> Result<patch::Patch, patch::PatchError> {
    patch::build_patch(original_len, read_original, table)
}

/// Operation: structural diff (roadmap item 43, slice R4c) —
/// compare two byte sources block-wise and report the coalesced
/// differing regions plus the first-divergence offset, windowed
/// so memory stays flat. See [`structdiff`].
pub fn op_structural_diff(
    len_a: u64,
    read_a: impl FnMut(u64, &mut [u8]) -> usize,
    len_b: u64,
    read_b: impl FnMut(u64, &mut [u8]) -> usize,
    block_size: u64,
) -> structdiff::StructDiffReport {
    structdiff::compare_sources(len_a, read_a, len_b, read_b, block_size)
}

/// Operation: find every match of a compiled regular expression
/// in the document (roadmap item 9, R5a core), streaming over
/// the piece table — the engine is a backtracking-free Pike VM,
/// so cost stays linear in the document size. See [`regex`].
pub fn op_find_regex(table: &PieceTable, re: &regex::Regex) -> Vec<(u64, u64)> {
    re.find_iter(table.len(), |offset, out| table.read_into(offset, out))
        .collect()
}

/// Operation: line diff (R5a core, for the side-by-side
/// streaming diff) — compare two line sources and report the
/// coalesced differing regions. See [`linediff`] for the
/// reader contract and the algorithm's stated limits.
pub fn op_line_diff(
    left_count: u64,
    read_left: impl FnMut(u64, &mut Vec<u8>) -> bool,
    right_count: u64,
    read_right: impl FnMut(u64, &mut Vec<u8>) -> bool,
) -> linediff::LineDiffReport {
    linediff::compare_lines(left_count, read_left, right_count, read_right)
}

/// Operation: lay a binary template over the document at `base`
/// and report every field's offset, size and decoded value
/// (roadmap item 17, R5a core). See [`templates`].
pub fn op_apply_template(
    table: &PieceTable,
    set: &templates::TemplateSet,
    name: &str,
    base: u64,
) -> Result<templates::AppliedStruct, templates::ApplyError> {
    set.apply(name, base, |offset, out| table.read_into(offset, out))
}

/// Operation: apply a multi-range edit as ONE undoable history
/// entry (R5a core — the multi-cursor primitive). See
/// [`undo::UndoStack::do_multi_replace`] for the ordering and
/// validation rules.
pub fn op_multi_replace(
    table: &mut PieceTable,
    history: &mut UndoStack,
    edits: &[undo::RangeEdit],
) -> Result<Option<undo::EditAction>, undo::MultiEditError> {
    history.do_multi_replace(table, edits)
}

/// Operation: extract a byte-column rectangle from a line range
/// (R5a core). `line_starts` is the byte offset of each line's
/// first byte. See [`undo::rect_extract`].
pub fn op_rect_extract(
    table: &PieceTable,
    line_starts: &[u64],
    first_line: usize,
    last_line: usize,
    col_start: usize,
    col_end: usize,
) -> Result<Vec<Vec<u8>>, undo::RectError> {
    undo::rect_extract(
        table,
        line_starts,
        first_line,
        last_line,
        col_start,
        col_end,
    )
}

/// Operation: delete a byte-column rectangle from a line range
/// as ONE undoable history entry (R5a core). See
/// [`undo::UndoStack::do_rect_delete`].
pub fn op_rect_delete(
    table: &mut PieceTable,
    history: &mut UndoStack,
    line_starts: &[u64],
    first_line: usize,
    last_line: usize,
    col_start: usize,
    col_end: usize,
) -> Result<Option<undo::EditAction>, undo::RectError> {
    history.do_rect_delete(
        table,
        line_starts,
        first_line,
        last_line,
        col_start,
        col_end,
    )
}

/// Operation: insert bytes at one byte column on every line of
/// a range as ONE undoable history entry (R5a core). See
/// [`undo::UndoStack::do_rect_insert`].
pub fn op_rect_insert(
    table: &mut PieceTable,
    history: &mut UndoStack,
    line_starts: &[u64],
    first_line: usize,
    last_line: usize,
    col: usize,
    bytes: &[u8],
) -> Result<Option<undo::EditAction>, undo::RectError> {
    history.do_rect_insert(table, line_starts, first_line, last_line, col, bytes)
}
