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

use crate::piece_table::{PieceNode, PieceTable};

/// Default maximum number of edits kept in history.
///
/// SPEC.md section 5.2 requires a memory-constrained history but does not
/// name a number, so this is a placeholder default until the cap is
/// confirmed; it can be overridden with [`UndoStack::with_capacity`].
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

/// Bounded undo/redo history for one document.
#[derive(Debug)]
pub struct UndoStack {
    undo: VecDeque<HistoryEntry>,
    redo: VecDeque<HistoryEntry>,
    capacity: usize,
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

    /// A history keeping at most `capacity` edits. `0` disables history:
    /// edits still apply, but nothing is recorded.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            capacity,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
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
        let (anchor, before) = insert_anchor(table, offset);
        table.insert(offset, bytes);
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

    /// Apply a delete to `table` and record it for undo. Returns the removed
    /// content pieces, exactly as [`PieceTable::delete`] reports them.
    pub fn do_delete(&mut self, table: &mut PieceTable, offset: u64, len: u64) -> Vec<PieceNode> {
        if len == 0 {
            return Vec::new();
        }
        let count_before = table.pieces.len();
        let (anchor, before) = delete_anchor(table, offset, len);
        let removed = table.delete(offset, len);
        if removed.is_empty() {
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
        let (anchor, before) = segment_anchor(table, start, end);
        let action = edit(table);
        let after = segment_after(table, anchor, before.len(), count_before);
        self.record(HistoryEntry {
            action: action.clone(),
            anchor,
            before,
            after,
        });
        action
    }

    /// Undo the most recent edit. Returns `false` when there is nothing to
    /// undo. Restores the piece layout exactly; the append buffer is not
    /// touched (SPEC.md section 5.2).
    pub fn apply_undo(&mut self, table: &mut PieceTable) -> bool {
        let Some(entry) = self.undo.pop_back() else {
            return false;
        };
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
        self.undo.push_back(entry);
        true
    }

    fn record(&mut self, entry: HistoryEntry) {
        // A fresh edit invalidates the redo future.
        self.redo.clear();
        if self.capacity == 0 {
            return;
        }
        self.undo.push_back(entry);
        while self.undo.len() > self.capacity {
            self.undo.pop_front();
        }
    }
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
    debug_assert!(table.check_invariants());
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
}
