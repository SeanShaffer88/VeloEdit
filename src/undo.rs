//! Transactional undo / redo engine (SPEC.md section 5.2).
//!
//! Mutations are stored as lightweight inversion deltas ([`EditAction`]).
//! Undo restores the previous piece layout directly; the bytes in the
//! append buffer are never altered or removed by undo or redo — an undone
//! insert simply leaves its bytes orphaned in the append buffer until new
//! edits append after them.
//!
//! Exact layout restoration: each history entry also records the small
//! segment of pieces the edit touched (the piece containing the edit point,
//! or every piece overlapping a deletion), before and after the edit. Undo
//! splices the "before" segment back in, redo splices the "after" segment.
//! Only the touched segment is stored, so history stays proportional to the
//! size of each edit's neighbourhood, not to the document.

use std::collections::VecDeque;

use crate::piece_table::{PieceNode, PieceTable, AUTO_COALESCE_THRESHOLD};

/// Default maximum number of edits kept in history.
///
/// SPEC.md section 5.2 requires a memory-constrained history but does not
/// name a number. The default is 1,024 steps; the owner can override it
/// (and the byte budget below) in `data/settings.json` — see
/// [`UndoStack::with_limits`] and `crate::settings`.
pub const DEFAULT_HISTORY_LIMIT: usize = 1024;

/// One reversible mutation (SPEC.md section 5.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditAction {
    /// Bytes were inserted at `offset`; undo deletes `length` bytes there.
    Insert { offset: u64, length: u64 },
    /// Bytes were deleted at `offset`; `previous_pieces` are the removed
    /// content pieces (clipped to the deleted range) in document order.
    Delete {
        offset: u64,
        previous_pieces: Vec<PieceNode>,
    },
    /// A replacement: one range, or a whole replace-all batch recorded as
    /// a single history entry. `offset` is the first replaced offset in
    /// pre-edit coordinates; `previous_pieces` are every removed content
    /// piece across all replaced ranges, in document order;
    /// `inserted_length` is the total number of replacement bytes
    /// inserted; `replacements` counts the replaced ranges (1 for a
    /// single replace).
    Replace {
        offset: u64,
        previous_pieces: Vec<PieceNode>,
        inserted_length: u64,
        replacements: usize,
    },
}

// Layout pins (architectural audit): the history bounds — at most
// `DEFAULT_HISTORY_LIMIT` steps and a byte budget over the stored
// payloads (see [`entry_payload`]) — are spent against records of
// exactly these sizes. If a variant or field change grows either
// record, the build breaks here instead of silently weakening the
// memory bounds the budget math assumes.
const _: () = assert!(std::mem::size_of::<EditAction>() == 48);
const _: () = assert!(std::mem::size_of::<HistoryEntry>() == 104);

/// One range replacement within a multi-range edit (see
/// [`UndoStack::do_multi_replace`]): the bytes
/// `[offset, offset + delete_len)` are replaced by `replacement`
/// (either side may be empty: a pure insertion, a pure
/// deletion).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeEdit<'a> {
    pub offset: u64,
    pub delete_len: u64,
    pub replacement: &'a [u8],
}

/// Why a multi-range edit was refused. Nothing is applied when
/// one comes back: validation runs before any mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MultiEditError {
    /// An edit starts before its predecessor starts.
    Unsorted { index: usize },
    /// An edit overlaps its predecessor (including starting at
    /// the same offset, where the order would be ambiguous).
    Overlapping { index: usize },
    /// An edit reaches outside the document.
    OutOfBounds { index: usize },
}

impl std::fmt::Display for MultiEditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MultiEditError::Unsorted { index } => {
                write!(f, "edit {index} starts before the edit before it (edits must be in document order)")
            }
            MultiEditError::Overlapping { index } => {
                write!(f, "edit {index} overlaps the edit before it")
            }
            MultiEditError::OutOfBounds { index } => {
                write!(f, "edit {index} reaches outside the document")
            }
        }
    }
}

impl std::error::Error for MultiEditError {}

/// Why a rectangular edit was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RectError {
    /// The line range is outside the supplied line map (or
    /// reversed).
    LineRange,
    /// `col_start` is greater than `col_end`.
    Columns,
    /// The constructed range edits were refused (cannot happen
    /// for a well-formed line map; carried for completeness).
    Edit(MultiEditError),
}

impl std::fmt::Display for RectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RectError::LineRange => write!(f, "the line range is outside the document's lines"),
            RectError::Columns => write!(f, "the column range is reversed"),
            RectError::Edit(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RectError {}

/// Internal history record: the logical action plus the exact piece segment
/// needed to restore the layout in either direction.
#[derive(Debug)]
struct HistoryEntry {
    action: EditAction,
    /// Index of the first piece in the touched segment.
    anchor: usize,
    /// Pieces in the touched segment before the edit.
    before: Vec<PieceNode>,
    /// Pieces in the touched segment after the edit.
    after: Vec<PieceNode>,
}

/// The stored weight of one history entry: the document bytes its
/// piece segments span (before + after) plus the action's own stored
/// bytes. This is the quantity the byte budget bounds — a proxy for
/// the memory a delta pins, so one huge edit (a replace-all over a
/// mass of pieces) counts far more than a keystroke.
fn entry_payload(entry: &HistoryEntry) -> u64 {
    let pieces_bytes = |pieces: &[PieceNode]| pieces.iter().map(|p| p.length).sum::<u64>();
    let action_bytes = match &entry.action {
        EditAction::Insert { length, .. } => *length,
        EditAction::Delete {
            previous_pieces, ..
        } => pieces_bytes(previous_pieces),
        EditAction::Replace {
            previous_pieces,
            inserted_length,
            ..
        } => pieces_bytes(previous_pieces) + inserted_length,
    };
    pieces_bytes(&entry.before) + pieces_bytes(&entry.after) + action_bytes
}

/// Bounded undo/redo history for one document.
///
/// Two bounds apply together (roadmap item 6): at most `capacity`
/// steps, and at most `byte_budget` summed payload bytes across the
/// undo stack. Whichever bound is hit first trims the oldest entries;
/// the newest entry is never trimmed, so the most recent edit can
/// always be undone even when it alone exceeds the budget.
#[derive(Debug)]
pub struct UndoStack {
    undo: VecDeque<HistoryEntry>,
    redo: VecDeque<HistoryEntry>,
    capacity: usize,
    byte_budget: u64,
    /// Summed [`entry_payload`] of the entries currently in `undo`,
    /// kept incrementally so trimming never rescans the stack.
    undo_bytes: u64,
}

impl Default for UndoStack {
    fn default() -> Self {
        Self::new()
    }
}

impl UndoStack {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_HISTORY_LIMIT)
    }

    /// A history keeping at most `capacity` edits, with no byte
    /// budget. `0` disables history: edits still apply, but nothing is
    /// recorded.
    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_limits(capacity, u64::MAX)
    }

    /// A history bounded by both a step limit and a byte budget (the
    /// sum of the stored deltas' payload bytes, see [`entry_payload`]).
    pub fn with_limits(capacity: usize, byte_budget: u64) -> Self {
        Self {
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            capacity,
            byte_budget,
            undo_bytes: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn byte_budget(&self) -> u64 {
        self.byte_budget
    }

    /// The summed payload bytes of the entries currently undoable.
    pub fn undo_bytes(&self) -> u64 {
        self.undo_bytes
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }

    /// Forget all recorded history in both directions.
    ///
    /// The atomic save calls this: saving is a history boundary (the
    /// append-buffer reset in SPEC.md section 5.1 step 6 invalidates the
    /// piece segments history entries point at), so nothing recorded
    /// before a save may be undone after it. See `crate::save`.
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.undo_bytes = 0;
    }

    /// The most recent undoable action, if any (newest first inspection).
    pub fn peek_undo(&self) -> Option<&EditAction> {
        self.undo.back().map(|entry| &entry.action)
    }

    /// Apply an insert to `table` and record it for undo.
    pub fn do_insert(&mut self, table: &mut PieceTable, offset: u64, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let count_before = table.pieces.len();
        let gen_before = table.coalesce_gen();
        let full_before = layout_snapshot(table, 2);
        let (anchor, before) = insert_anchor(table, offset);
        table.insert(offset, bytes);
        if table.coalesce_gen() != gen_before {
            let full_before = full_before.expect("coalesce implies the snapshot was taken");
            return self.record(HistoryEntry {
                action: EditAction::Insert {
                    offset,
                    length: bytes.len() as u64,
                },
                anchor: 0,
                before: full_before,
                after: table.pieces.clone(),
            });
        }
        let after = segment_after(table, anchor, before.len(), count_before);
        self.record(HistoryEntry {
            action: EditAction::Insert {
                offset,
                length: bytes.len() as u64,
            },
            anchor,
            before,
            after,
        });
    }

    /// Apply a zero-run insert to `table` and record it for undo
    /// (roadmap R4 zero-run insert). The recorded action is an
    /// ordinary [`EditAction::Insert`] of `count` bytes — the bytes
    /// happen to be zeros — and the segment bookkeeping is the same
    /// as [`Self::do_insert`]'s, so undo restores the exact prior
    /// layout (including splitting an extended zero piece back to
    /// its old length) and redo re-applies it.
    pub fn do_insert_zeroes(&mut self, table: &mut PieceTable, offset: u64, count: u64) {
        if count == 0 {
            return;
        }
        let count_before = table.pieces.len();
        let gen_before = table.coalesce_gen();
        let full_before = layout_snapshot(table, 2);
        let (anchor, before) = insert_anchor(table, offset);
        table.insert_zeroes(offset, count);
        if table.coalesce_gen() != gen_before {
            let full_before = full_before.expect("coalesce implies the snapshot was taken");
            return self.record(HistoryEntry {
                action: EditAction::Insert {
                    offset,
                    length: count,
                },
                anchor: 0,
                before: full_before,
                after: table.pieces.clone(),
            });
        }
        let after = segment_after(table, anchor, before.len(), count_before);
        self.record(HistoryEntry {
            action: EditAction::Insert {
                offset,
                length: count,
            },
            anchor,
            before,
            after,
        });
    }

    /// Apply a delete to `table` and record it for undo. Returns the removed
    /// content pieces, exactly as [`PieceTable::delete`] reports them.
    pub fn do_delete(&mut self, table: &mut PieceTable, offset: u64, len: u64) -> Vec<PieceNode> {
        if len == 0 {
            return Vec::new();
        }
        let count_before = table.pieces.len();
        let gen_before = table.coalesce_gen();
        let full_before = layout_snapshot(table, 0);
        let (anchor, before) = delete_anchor(table, offset, len);
        let removed = table.delete(offset, len);
        if removed.is_empty() {
            return removed;
        }
        if table.coalesce_gen() != gen_before {
            let full_before = full_before.expect("coalesce implies the snapshot was taken");
            self.record(HistoryEntry {
                action: EditAction::Delete {
                    offset,
                    previous_pieces: removed.clone(),
                },
                anchor: 0,
                before: full_before,
                after: table.pieces.clone(),
            });
            return removed;
        }
        let after = segment_after(table, anchor, before.len(), count_before);
        self.record(HistoryEntry {
            action: EditAction::Delete {
                offset,
                previous_pieces: removed.clone(),
            },
            anchor,
            before,
            after,
        });
        removed
    }

    /// Apply a composite edit that changes only pieces overlapping the
    /// range `[start, end)` and record it as ONE history entry (used by
    /// replace and replace-all in `crate::search`). `edit` performs the
    /// change on the table and returns the [`EditAction`] describing it;
    /// that action is recorded and also returned here.
    ///
    /// The recorded segment is every piece overlapping `[start, end)`,
    /// extended one piece to the left. The extension matters: an insert
    /// inside the edit can *extend* the piece ending exactly at an insert
    /// offset through the coalescence rule (SPEC.md section 2.2) instead
    /// of adding a node, and that piece can sit just before the edited
    /// range. If it were outside the recorded segment, undo would splice
    /// the old segment back but keep the extension, corrupting the
    /// document. Recording it costs one extra small node per entry.
    pub fn do_segment_replace(
        &mut self,
        table: &mut PieceTable,
        start: u64,
        end: u64,
        edit: impl FnOnce(&mut PieceTable) -> EditAction,
    ) -> EditAction {
        debug_assert!(start < end && end <= table.len());
        let count_before = table.pieces.len();
        let gen_before = table.coalesce_gen();
        // A segment replace performs one delete+insert pair per
        // replaced range inside `[start, end)`, so it can add at most
        // two pieces per byte of the segment (a 1-byte needle replaced
        // at every byte) plus slack for the boundary splits.
        let full_before = layout_snapshot(table, 2 * (end - start) + 4);
        let (anchor, before) = segment_anchor(table, start, end);
        let action = edit(table);
        if table.coalesce_gen() != gen_before {
            let full_before = full_before.expect("coalesce implies the snapshot was taken");
            self.record(HistoryEntry {
                action: action.clone(),
                anchor: 0,
                before: full_before,
                after: table.pieces.clone(),
            });
            return action;
        }
        let after = segment_after(table, anchor, before.len(), count_before);
        self.record(HistoryEntry {
            action: action.clone(),
            anchor,
            before,
            after,
        });
        action
    }

    /// Apply a multi-range edit — many range replacements at
    /// once — as ONE history entry (the multi-cursor core: every
    /// cursor's replacement lands together, and one undo takes
    /// them all back). `edits` must be in document order:
    /// strictly ascending offsets, non-overlapping. They are
    /// validated before anything mutates; a refused edit changes
    /// nothing and records nothing. No-op edits (delete nothing,
    /// insert nothing) are dropped; if none remain, nothing is
    /// recorded and `Ok(None)` comes back.
    pub fn do_multi_replace(
        &mut self,
        table: &mut PieceTable,
        edits: &[RangeEdit],
    ) -> Result<Option<EditAction>, MultiEditError> {
        let doc_len = table.len();
        for (index, edit) in edits.iter().enumerate() {
            if edit.offset > doc_len || edit.delete_len > doc_len - edit.offset {
                return Err(MultiEditError::OutOfBounds { index });
            }
            if index > 0 {
                let prev = &edits[index - 1];
                if edit.offset < prev.offset {
                    return Err(MultiEditError::Unsorted { index });
                }
                if edit.offset == prev.offset || prev.offset + prev.delete_len > edit.offset {
                    return Err(MultiEditError::Overlapping { index });
                }
            }
        }
        let edits: Vec<RangeEdit> = edits
            .iter()
            .copied()
            .filter(|e| e.delete_len > 0 || !e.replacement.is_empty())
            .collect();
        if edits.is_empty() {
            return Ok(None);
        }
        // A single pure insertion is exactly do_insert's shape.
        if edits.len() == 1 && edits[0].delete_len == 0 {
            let edit = edits[0];
            self.do_insert(table, edit.offset, edit.replacement);
            return Ok(Some(EditAction::Insert {
                offset: edit.offset,
                length: edit.replacement.len() as u64,
            }));
        }
        let first = edits[0].offset;
        let last = &edits[edits.len() - 1];
        let last_end = last.offset + last.delete_len;
        let total_inserted: u64 = edits.iter().map(|e| e.replacement.len() as u64).sum();
        let count = edits.len();
        let action = self.do_segment_replace(table, first, last_end, |t| {
            let mut removed_groups: Vec<Vec<PieceNode>> = Vec::with_capacity(count);
            // Back to front: edits at higher offsets do not move
            // the offsets of edits still to be applied.
            for edit in edits.iter().rev() {
                let removed = t.delete(edit.offset, edit.delete_len);
                removed_groups.push(removed);
                t.insert(edit.offset, edit.replacement);
            }
            let previous_pieces: Vec<PieceNode> = removed_groups
                .into_iter()
                .rev()
                .flat_map(|group| group.into_iter())
                .collect();
            EditAction::Replace {
                offset: first,
                previous_pieces,
                inserted_length: total_inserted,
                replacements: count,
            }
        });
        Ok(Some(action))
    }

    /// Delete the column range `[col_start, col_end)` (byte
    /// columns, clamped per line) from every line in
    /// `[first_line, last_line]`, as ONE history entry. Lines
    /// are addressed through `line_starts` (the byte offset of
    /// each line's first byte, ascending, as a line index
    /// reports them); a line's content excludes its `\n`
    /// terminator, which is never touched. Lines shorter than
    /// the column range contribute nothing.
    pub fn do_rect_delete(
        &mut self,
        table: &mut PieceTable,
        line_starts: &[u64],
        first_line: usize,
        last_line: usize,
        col_start: usize,
        col_end: usize,
    ) -> Result<Option<EditAction>, RectError> {
        check_rect_lines(line_starts, first_line, last_line)?;
        if col_start > col_end {
            return Err(RectError::Columns);
        }
        let mut edits = Vec::new();
        for line in first_line..=last_line {
            let (start, end) = line_content_span(table, line_starts, line);
            let len = end - start;
            let cs = (col_start as u64).min(len);
            let ce = (col_end as u64).min(len);
            if ce > cs {
                edits.push(RangeEdit {
                    offset: start + cs,
                    delete_len: ce - cs,
                    replacement: &[],
                });
            }
        }
        self.do_multi_replace(table, &edits)
            .map_err(RectError::Edit)
    }

    /// Insert `bytes` at byte column `col` (clamped to each
    /// line's content length — a shorter line takes the bytes at
    /// its end, with no padding) on every line in
    /// `[first_line, last_line]`, as ONE history entry.
    pub fn do_rect_insert(
        &mut self,
        table: &mut PieceTable,
        line_starts: &[u64],
        first_line: usize,
        last_line: usize,
        col: usize,
        bytes: &[u8],
    ) -> Result<Option<EditAction>, RectError> {
        check_rect_lines(line_starts, first_line, last_line)?;
        if bytes.is_empty() {
            return Ok(None);
        }
        let mut edits = Vec::new();
        for line in first_line..=last_line {
            let (start, end) = line_content_span(table, line_starts, line);
            let at = start + (col as u64).min(end - start);
            edits.push(RangeEdit {
                offset: at,
                delete_len: 0,
                replacement: bytes,
            });
        }
        self.do_multi_replace(table, &edits)
            .map_err(RectError::Edit)
    }

    /// Undo the most recent edit. Returns `false` when there is nothing to
    /// undo. Restores the piece layout exactly; the append buffer is not
    /// touched (SPEC.md section 5.2).
    pub fn apply_undo(&mut self, table: &mut PieceTable) -> bool {
        let Some(entry) = self.undo.pop_back() else {
            return false;
        };
        self.undo_bytes -= entry_payload(&entry);
        splice_segment(table, entry.anchor, entry.after.len(), &entry.before);
        self.redo.push_back(entry);
        true
    }

    /// Redo the most recently undone edit. Returns `false` when there is
    /// nothing to redo. The append buffer is not touched.
    pub fn apply_redo(&mut self, table: &mut PieceTable) -> bool {
        let Some(entry) = self.redo.pop_back() else {
            return false;
        };
        splice_segment(table, entry.anchor, entry.before.len(), &entry.after);
        self.undo_bytes += entry_payload(&entry);
        self.undo.push_back(entry);
        true
    }

    fn record(&mut self, entry: HistoryEntry) {
        // A fresh edit invalidates the redo future.
        self.redo.clear();
        if self.capacity == 0 {
            return;
        }
        self.undo_bytes += entry_payload(&entry);
        self.undo.push_back(entry);
        // Trim the oldest entries until both bounds hold. The newest
        // entry is never trimmed: the loop stops at one entry, so the
        // edit just made can always be undone, even when it alone is
        // bigger than the whole byte budget.
        while self.undo.len() > 1
            && (self.undo.len() > self.capacity || self.undo_bytes > self.byte_budget)
        {
            let dropped = self.undo.pop_front().expect("len > 1");
            self.undo_bytes -= entry_payload(&dropped);
        }
    }
}

/// Validate a rectangular edit's line range against the line
/// map.
fn check_rect_lines(
    line_starts: &[u64],
    first_line: usize,
    last_line: usize,
) -> Result<(), RectError> {
    if first_line > last_line || last_line >= line_starts.len() {
        return Err(RectError::LineRange);
    }
    Ok(())
}

/// The content span of one line: `[start, end)` excluding the
/// `\n` terminator (a `\r` before it is content, byte-honestly).
/// The last line's raw end is the document's length.
fn line_content_span(table: &PieceTable, line_starts: &[u64], line: usize) -> (u64, u64) {
    let start = line_starts[line];
    let raw_end = if line + 1 < line_starts.len() {
        line_starts[line + 1]
    } else {
        table.len()
    };
    let end = if raw_end > start && table.byte_at(raw_end - 1) == Some(b'\n') {
        raw_end - 1
    } else {
        raw_end
    };
    (start, end)
}

/// Extract the rectangle covering byte columns
/// `[col_start, col_end)` of lines `[first_line, last_line]`:
/// one entry per line, the line's bytes inside the columns —
/// clamped, so a line shorter than `col_start` contributes an
/// empty entry and a line ending inside the range contributes
/// what it has. A pure read; nothing is recorded or changed.
pub fn rect_extract(
    table: &PieceTable,
    line_starts: &[u64],
    first_line: usize,
    last_line: usize,
    col_start: usize,
    col_end: usize,
) -> Result<Vec<Vec<u8>>, RectError> {
    check_rect_lines(line_starts, first_line, last_line)?;
    if col_start > col_end {
        return Err(RectError::Columns);
    }
    let mut out = Vec::with_capacity(last_line - first_line + 1);
    for line in first_line..=last_line {
        let (start, end) = line_content_span(table, line_starts, line);
        let len = end - start;
        let cs = (col_start as u64).min(len);
        let ce = (col_end as u64).min(len);
        out.push(table.read(start + cs, ce - cs));
    }
    Ok(out)
}

/// Replace `segment_len` pieces starting at `anchor` with `replacement` and
/// recompute the total length from the piece list.
fn splice_segment(
    table: &mut PieceTable,
    anchor: usize,
    segment_len: usize,
    replacement: &[PieceNode],
) {
    let removed: Vec<PieceNode> = table
        .pieces
        .splice(anchor..anchor + segment_len, replacement.iter().copied())
        .collect();
    drop(removed);
    table.total_length = table.pieces.iter().map(|p| p.length).sum();
    // This splice rewrote `pieces` directly, outside the piece table's
    // own mutation paths: resync its Fenwick weight index.
    table.rebuild_weights();
    debug_assert!(table.check_invariants());
}

/// Snapshot the full piece layout when an edit could trigger the
/// piece table's automatic coalesce pass (roadmap item 2): the pass
/// fires when the post-edit count exceeds
/// [`AUTO_COALESCE_THRESHOLD`], and `headroom` bounds how many pieces
/// the edit can add (2 for a single insert, 0 for a delete, two per
/// replaced range for a segment replace). A coalesce merges pieces
/// *anywhere* in the table, which the segment bookkeeping in this
/// module cannot attribute to the touched segment; when one fires
/// inside a recorded edit, the entry falls back to a whole-layout
/// before/after pair (correct, and rare — ordinary typing coalescence
/// never triggers the pass).
fn layout_snapshot(table: &PieceTable, headroom: u64) -> Option<Vec<PieceNode>> {
    if table.pieces.len() as u64 + headroom >= AUTO_COALESCE_THRESHOLD as u64 {
        Some(table.pieces.clone())
    } else {
        None
    }
}

/// The segment an edit produced, derived from the touched segment length
/// plus the change in the total piece count (an edit only changes pieces
/// inside its touched segment, so the segment grows or shrinks by exactly
/// the global delta).
fn segment_after(
    table: &PieceTable,
    anchor: usize,
    before_len: usize,
    count_before: usize,
) -> Vec<PieceNode> {
    let delta = table.pieces.len() as isize - count_before as isize;
    let after_len = (before_len as isize + delta) as usize;
    table.pieces[anchor..anchor + after_len].to_vec()
}

/// Anchor segment for an insert at `offset`: the piece ending exactly at
/// `offset` (the coalescence / boundary case), or the piece containing
/// `offset`, or the first piece when inserting at offset 0.
fn insert_anchor(table: &PieceTable, offset: u64) -> (usize, Vec<PieceNode>) {
    let mut start: u64 = 0;
    for (idx, piece) in table.pieces.iter().enumerate() {
        let end = start + piece.length;
        if offset == end || (offset > start && offset < end) || (offset == 0 && start == 0) {
            return (idx, vec![*piece]);
        }
        start = end;
    }
    (table.pieces.len(), Vec::new())
}

/// Anchor segment for a composite edit over `[start, end)`: every piece
/// overlapping the range (as [`delete_anchor`] computes), extended one
/// piece to the left when there is one, so a coalescing insert at the
/// start of the range extends a recorded piece — never an unrecorded
/// one (see [`UndoStack::do_segment_replace`]).
fn segment_anchor(table: &PieceTable, start: u64, end: u64) -> (usize, Vec<PieceNode>) {
    let (mut anchor, mut before) = delete_anchor(table, start, end - start);
    if anchor > 0 && !before.is_empty() {
        anchor -= 1;
        before.insert(0, table.pieces[anchor]);
    }
    (anchor, before)
}

/// Anchor segment for a delete of `[offset, offset + len)`: every piece
/// overlapping the range, starting at the first overlapping piece.
fn delete_anchor(table: &PieceTable, offset: u64, len: u64) -> (usize, Vec<PieceNode>) {
    let del_end = offset + len;
    let mut start: u64 = 0;
    let mut anchor: Option<usize> = None;
    let mut before: Vec<PieceNode> = Vec::new();
    for (idx, piece) in table.pieces.iter().enumerate() {
        let end = start + piece.length;
        if end > offset && start < del_end {
            if anchor.is_none() {
                anchor = Some(idx);
            }
            before.push(*piece);
        }
        start = end;
    }
    (anchor.unwrap_or(table.pieces.len()), before)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::piece_table::BufferSource;

    fn text(table: &PieceTable) -> String {
        String::from_utf8(table.read(0, table.len())).unwrap()
    }

    #[test]
    fn undo_insert_restores_text_and_layout() {
        let mut table = PieceTable::from_original(b"hello world".to_vec());
        let layout_before = table.pieces.clone();
        let mut history = UndoStack::new();
        history.do_insert(&mut table, 5, b" brave new");
        assert_eq!(text(&table), "hello brave new world");
        assert!(history.apply_undo(&mut table));
        assert_eq!(text(&table), "hello world");
        assert_eq!(table.pieces, layout_before);
        assert!(table.check_invariants());
    }

    #[test]
    fn redo_insert_reapplies_exact_layout() {
        let mut table = PieceTable::from_original(b"ac".to_vec());
        let mut history = UndoStack::new();
        history.do_insert(&mut table, 1, b"b");
        let layout_after = table.pieces.clone();
        assert!(history.apply_undo(&mut table));
        assert!(history.apply_redo(&mut table));
        assert_eq!(text(&table), "abc");
        assert_eq!(table.pieces, layout_after);
        assert!(table.check_invariants());
    }

    #[test]
    fn undo_delete_restores_split_pieces_exactly() {
        let mut table = PieceTable::from_original(b"hello world".to_vec());
        let mut history = UndoStack::new();
        // First make a multi-piece layout, then delete across the seams.
        history.do_insert(&mut table, 5, b"---");
        let layout_before = table.pieces.clone();
        let removed = history.do_delete(&mut table, 3, 8);
        assert_eq!(removed.iter().map(|p| p.length).sum::<u64>(), 8);
        assert_ne!(table.pieces, layout_before);
        assert!(history.apply_undo(&mut table));
        assert_eq!(table.pieces, layout_before);
        assert_eq!(text(&table), "hello--- world");
        // And redo deletes the same range again.
        assert!(history.apply_redo(&mut table));
        assert_eq!(text(&table), "helrld");
        assert!(table.check_invariants());
    }

    #[test]
    fn undo_never_touches_append_buffer() {
        let mut table = PieceTable::from_original(b"base".to_vec());
        let mut history = UndoStack::new();
        history.do_insert(&mut table, 4, b"+added");
        let buffer_after_insert = table.append_buffer.clone();
        assert!(history.apply_undo(&mut table));
        assert_eq!(table.append_buffer, buffer_after_insert);
        assert!(history.apply_redo(&mut table));
        assert_eq!(table.append_buffer, buffer_after_insert);
        // Even undoing a delete of Add bytes leaves the buffer alone.
        history.do_delete(&mut table, 4, 6);
        assert!(history.apply_undo(&mut table));
        assert_eq!(table.append_buffer, buffer_after_insert);
    }

    #[test]
    fn round_trip_sequence_of_mixed_edits() {
        let mut table = PieceTable::from_original(b"The quick brown fox".to_vec());
        let mut history = UndoStack::new();
        let original = text(&table);
        history.do_insert(&mut table, 4, b"very ");
        history.do_delete(&mut table, 15, 6);
        history.do_insert(&mut table, 0, b">> ");
        assert_eq!(text(&table), ">> The very quick fox");
        assert!(history.apply_undo(&mut table));
        assert!(history.apply_undo(&mut table));
        assert!(history.apply_undo(&mut table));
        assert_eq!(text(&table), original);
        assert!(!history.can_undo());
        assert!(history.apply_redo(&mut table));
        assert!(history.apply_redo(&mut table));
        assert!(history.apply_redo(&mut table));
        assert_eq!(text(&table), ">> The very quick fox");
        assert!(table.check_invariants());
    }

    #[test]
    fn coalesced_typing_undoes_as_one_history_entry() {
        let mut table = PieceTable::from_original(Vec::new());
        let mut history = UndoStack::new();
        history.do_insert(&mut table, 0, b"a");
        history.do_insert(&mut table, 1, b"b");
        // The second insert coalesced into the same piece; undoing it must
        // restore the one-piece layout with length 1, not remove a node.
        assert_eq!(table.piece_count(), 1);
        assert!(history.apply_undo(&mut table));
        assert_eq!(text(&table), "a");
        assert_eq!(table.piece_count(), 1);
        assert!(history.apply_undo(&mut table));
        assert_eq!(text(&table), "");
        assert_eq!(table.piece_count(), 0);
    }

    #[test]
    fn new_edit_clears_redo() {
        let mut table = PieceTable::from_original(b"x".to_vec());
        let mut history = UndoStack::new();
        history.do_insert(&mut table, 1, b"y");
        assert!(history.apply_undo(&mut table));
        assert!(history.can_redo());
        history.do_insert(&mut table, 1, b"z");
        assert!(!history.can_redo());
        assert_eq!(text(&table), "xz");
    }

    #[test]
    fn history_is_bounded_by_capacity() {
        let mut table = PieceTable::from_original(Vec::new());
        let mut history = UndoStack::with_capacity(3);
        for i in 0..5u8 {
            let offset = table.len();
            history.do_insert(&mut table, offset, &[b'a' + i]);
        }
        assert_eq!(history.undo_len(), 3);
        // Only the three newest edits can be undone.
        assert!(history.apply_undo(&mut table));
        assert!(history.apply_undo(&mut table));
        assert!(history.apply_undo(&mut table));
        assert!(!history.apply_undo(&mut table));
        assert_eq!(text(&table), "ab"); // first two inserts fell off history
    }

    #[test]
    fn byte_budget_trims_oldest_first() {
        let mut table = PieceTable::from_original(Vec::new());
        // Budget 20: the first insert weighs 8 (4 segment bytes after
        // the edit + 4 action bytes), the second 16 (it coalesces
        // into the same piece: 4 before + 8 after + 4 action bytes).
        let mut history = UndoStack::with_limits(100, 20);
        history.do_insert(&mut table, 0, b"aaaa");
        assert_eq!(history.undo_len(), 1);
        assert_eq!(history.undo_bytes(), 8);
        history.do_insert(&mut table, 4, b"bbbb");
        // 8 + 16 = 24 > 20: the oldest entry is dropped, the newest
        // stays — only the second insert can still be undone.
        assert_eq!(history.undo_len(), 1);
        assert_eq!(history.undo_bytes(), 16);
        assert_eq!(
            history.peek_undo(),
            Some(&EditAction::Insert {
                offset: 4,
                length: 4
            })
        );
        assert!(history.apply_undo(&mut table));
        assert_eq!(text(&table), "aaaa");
        assert_eq!(history.undo_bytes(), 0);
        assert!(!history.apply_undo(&mut table));
    }

    #[test]
    fn byte_budget_never_drops_the_just_added_entry() {
        let mut table = PieceTable::from_original(Vec::new());
        // A budget smaller than a single edit: the edit is still
        // recorded and undoable (history never trims itself empty).
        let mut history = UndoStack::with_limits(100, 4);
        history.do_insert(&mut table, 0, b"aaaaaaaa");
        assert_eq!(history.undo_len(), 1);
        assert!(history.can_undo());
        assert!(history.apply_undo(&mut table));
        assert_eq!(text(&table), "");
    }

    #[test]
    fn default_stacks_have_no_byte_budget() {
        assert_eq!(UndoStack::new().byte_budget(), u64::MAX);
        assert_eq!(UndoStack::with_capacity(3).byte_budget(), u64::MAX);
        assert_eq!(UndoStack::with_limits(3, 99).byte_budget(), 99);
        assert_eq!(UndoStack::new().undo_bytes(), 0);
    }

    #[test]
    fn empty_history_and_zero_capacity() {
        let mut table = PieceTable::from_original(b"x".to_vec());
        let mut history = UndoStack::new();
        assert!(!history.apply_undo(&mut table));
        assert!(!history.apply_redo(&mut table));

        let mut disabled = UndoStack::with_capacity(0);
        disabled.do_insert(&mut table, 1, b"y");
        assert_eq!(text(&table), "xy");
        assert!(!disabled.can_undo());
        assert!(!disabled.apply_undo(&mut table));
    }

    #[test]
    fn undo_zero_run_insert_restores_layout_exactly() {
        let mut table = PieceTable::from_original(b"abcd".to_vec());
        let layout_before = table.pieces.clone();
        let mut history = UndoStack::new();
        history.do_insert_zeroes(&mut table, 2, 3);
        assert_eq!(table.read(0, 99), vec![b'a', b'b', 0, 0, 0, b'c', b'd']);
        assert!(history.apply_undo(&mut table));
        assert_eq!(table.pieces, layout_before);
        assert_eq!(text(&table), "abcd");
        assert!(history.apply_redo(&mut table));
        assert_eq!(table.read(0, 99), vec![b'a', b'b', 0, 0, 0, b'c', b'd']);
        // Extending an existing run also undoes to the old length:
        // the second insert extends the same piece in place, and
        // undo must shrink it back rather than remove a node.
        history.do_insert_zeroes(&mut table, 5, 4);
        assert_eq!(table.piece_count(), 3);
        assert_eq!(table.pieces[1].length, 7);
        assert!(history.apply_undo(&mut table));
        assert_eq!(table.piece_count(), 3);
        assert_eq!(table.pieces[1].length, 3);
        assert_eq!(table.read(0, 99), vec![b'a', b'b', 0, 0, 0, b'c', b'd']);
        assert!(table.check_invariants());
    }

    #[test]
    fn recorded_actions_match_spec_shape() {
        let mut table = PieceTable::from_original(b"hello world".to_vec());
        let mut history = UndoStack::new();
        history.do_insert(&mut table, 5, b"!");
        assert_eq!(
            history.peek_undo(),
            Some(&EditAction::Insert {
                offset: 5,
                length: 1
            })
        );
        let removed = history.do_delete(&mut table, 0, 2);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].source, BufferSource::Original);
        assert_eq!(
            history.peek_undo(),
            Some(&EditAction::Delete {
                offset: 0,
                previous_pieces: removed,
            })
        );
    }

    // ---- Multi-range + rectangular editing (R5a core) -------

    fn line_starts_of(bytes: &[u8]) -> Vec<u64> {
        let mut starts = vec![0];
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'\n' && i + 1 < bytes.len() {
                starts.push((i + 1) as u64);
            }
        }
        starts
    }

    #[test]
    fn multi_replace_is_one_undo_entry() {
        let mut table = PieceTable::from_original(b"hello world, hello moon".to_vec());
        let layout_before = table.pieces.clone();
        let mut history = UndoStack::new();
        let action = history
            .do_multi_replace(
                &mut table,
                &[
                    RangeEdit {
                        offset: 0,
                        delete_len: 5,
                        replacement: b"bye",
                    },
                    RangeEdit {
                        offset: 13,
                        delete_len: 5,
                        replacement: b"bye",
                    },
                ],
            )
            .unwrap();
        assert_eq!(text(&table), "bye world, bye moon");
        match action {
            Some(EditAction::Replace {
                replacements: 2,
                inserted_length: 6,
                ..
            }) => {}
            other => panic!("unexpected action {other:?}"),
        }
        assert_eq!(history.undo_len(), 1);
        assert!(history.apply_undo(&mut table));
        assert_eq!(text(&table), "hello world, hello moon");
        assert_eq!(table.pieces, layout_before);
        assert!(history.apply_redo(&mut table));
        assert_eq!(text(&table), "bye world, bye moon");
        assert!(table.check_invariants());
    }

    #[test]
    fn multi_insert_at_many_cursors_is_one_entry() {
        let mut table = PieceTable::from_original(b"a b c".to_vec());
        let mut history = UndoStack::new();
        history
            .do_multi_replace(
                &mut table,
                &[
                    RangeEdit {
                        offset: 0,
                        delete_len: 0,
                        replacement: b">",
                    },
                    RangeEdit {
                        offset: 2,
                        delete_len: 0,
                        replacement: b">",
                    },
                    RangeEdit {
                        offset: 4,
                        delete_len: 0,
                        replacement: b">",
                    },
                ],
            )
            .unwrap();
        assert_eq!(text(&table), ">a >b >c");
        assert_eq!(history.undo_len(), 1);
        assert!(history.apply_undo(&mut table));
        assert_eq!(text(&table), "a b c");
    }

    #[test]
    fn multi_replace_validation_refuses_without_mutating() {
        let mut table = PieceTable::from_original(b"hello world".to_vec());
        let layout_before = table.pieces.clone();
        let mut history = UndoStack::new();
        // Unsorted.
        assert_eq!(
            history.do_multi_replace(
                &mut table,
                &[
                    RangeEdit {
                        offset: 6,
                        delete_len: 1,
                        replacement: b"x"
                    },
                    RangeEdit {
                        offset: 2,
                        delete_len: 1,
                        replacement: b"x"
                    },
                ]
            ),
            Err(MultiEditError::Unsorted { index: 1 })
        );
        // Overlapping.
        assert_eq!(
            history.do_multi_replace(
                &mut table,
                &[
                    RangeEdit {
                        offset: 0,
                        delete_len: 5,
                        replacement: b"x"
                    },
                    RangeEdit {
                        offset: 3,
                        delete_len: 1,
                        replacement: b"x"
                    },
                ]
            ),
            Err(MultiEditError::Overlapping { index: 1 })
        );
        // Same start, even for two pure inserts: ambiguous order.
        assert_eq!(
            history.do_multi_replace(
                &mut table,
                &[
                    RangeEdit {
                        offset: 4,
                        delete_len: 0,
                        replacement: b"x"
                    },
                    RangeEdit {
                        offset: 4,
                        delete_len: 0,
                        replacement: b"y"
                    },
                ]
            ),
            Err(MultiEditError::Overlapping { index: 1 })
        );
        // Out of bounds: start past the end, and a delete running
        // past the end.
        assert_eq!(
            history.do_multi_replace(
                &mut table,
                &[RangeEdit {
                    offset: 12,
                    delete_len: 0,
                    replacement: b"x"
                },]
            ),
            Err(MultiEditError::OutOfBounds { index: 0 })
        );
        assert_eq!(
            history.do_multi_replace(
                &mut table,
                &[RangeEdit {
                    offset: 9,
                    delete_len: 5,
                    replacement: b"x"
                },]
            ),
            Err(MultiEditError::OutOfBounds { index: 0 })
        );
        // Nothing applied, nothing recorded.
        assert_eq!(text(&table), "hello world");
        assert_eq!(table.pieces, layout_before);
        assert!(!history.can_undo());
    }

    #[test]
    fn multi_replace_noops_record_nothing() {
        let mut table = PieceTable::from_original(b"abc".to_vec());
        let mut history = UndoStack::new();
        let result = history
            .do_multi_replace(
                &mut table,
                &[RangeEdit {
                    offset: 1,
                    delete_len: 0,
                    replacement: b"",
                }],
            )
            .unwrap();
        assert_eq!(result, None);
        assert!(!history.can_undo());
        // A single pure insert still records the ordinary way.
        let result = history
            .do_multi_replace(
                &mut table,
                &[RangeEdit {
                    offset: 3,
                    delete_len: 0,
                    replacement: b"d",
                }],
            )
            .unwrap();
        assert_eq!(
            result,
            Some(EditAction::Insert {
                offset: 3,
                length: 1
            })
        );
        assert_eq!(text(&table), "abcd");
        assert!(history.apply_undo(&mut table));
        assert_eq!(text(&table), "abc");
    }

    #[test]
    fn rect_extract_clamps_per_line() {
        let bytes = b"abc\nde\nfghi\nx";
        let table = PieceTable::from_original(bytes.to_vec());
        let starts = line_starts_of(bytes);
        assert_eq!(starts, vec![0, 4, 7, 12]);
        let out = rect_extract(&table, &starts, 0, 3, 1, 3).unwrap();
        assert_eq!(
            out,
            vec![b"bc".to_vec(), b"e".to_vec(), b"gh".to_vec(), b"".to_vec()]
        );
        // Columns fully past a short line: still one empty entry.
        let out = rect_extract(&table, &starts, 1, 1, 5, 9).unwrap();
        assert_eq!(out, vec![Vec::<u8>::new()]);
    }

    #[test]
    fn rect_delete_and_insert_are_single_undo_entries() {
        let bytes = b"abc\nde\nfghi";
        let starts = line_starts_of(bytes);
        let mut table = PieceTable::from_original(bytes.to_vec());
        let layout_before = table.pieces.clone();
        let mut history = UndoStack::new();
        history
            .do_rect_delete(&mut table, &starts, 0, 2, 0, 1)
            .unwrap();
        assert_eq!(text(&table), "bc\ne\nghi");
        assert_eq!(history.undo_len(), 1);
        assert!(history.apply_undo(&mut table));
        assert_eq!(text(&table), "abc\nde\nfghi");
        assert_eq!(table.pieces, layout_before);

        history
            .do_rect_insert(&mut table, &starts, 0, 2, 1, b">")
            .unwrap();
        assert_eq!(text(&table), "a>bc\nd>e\nf>ghi");
        assert_eq!(history.undo_len(), 1);
        assert!(history.apply_undo(&mut table));
        assert_eq!(text(&table), "abc\nde\nfghi");
        // Insert clamped to a short line's end: no padding.
        history
            .do_rect_insert(&mut table, &starts, 1, 1, 9, b"!")
            .unwrap();
        assert_eq!(text(&table), "abc\nde!\nfghi");
        assert!(table.check_invariants());
    }

    #[test]
    fn rect_errors_and_empty_column_range() {
        let bytes = b"abc\ndef";
        let starts = line_starts_of(bytes);
        let mut table = PieceTable::from_original(bytes.to_vec());
        let mut history = UndoStack::new();
        assert_eq!(
            history.do_rect_delete(&mut table, &starts, 2, 1, 0, 1),
            Err(RectError::LineRange)
        );
        assert_eq!(
            history.do_rect_delete(&mut table, &starts, 0, 5, 0, 1),
            Err(RectError::LineRange)
        );
        assert_eq!(
            history.do_rect_delete(&mut table, &starts, 0, 1, 3, 2),
            Err(RectError::Columns)
        );
        assert_eq!(
            rect_extract(&table, &starts, 0, 1, 2, 2).unwrap(),
            vec![Vec::<u8>::new(), Vec::<u8>::new()]
        );
        // A zero-width delete is a well-defined no-op.
        assert_eq!(
            history
                .do_rect_delete(&mut table, &starts, 0, 1, 1, 1)
                .unwrap(),
            None
        );
        assert!(!history.can_undo());
    }
}
