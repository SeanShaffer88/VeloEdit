//! Memory-mapped piece table engine (SPEC.md section 2).
//!
//! The document is never held as one contiguous edited buffer. The original
//! file bytes stay in a read-only backing store (on Windows, an mmap view
//! created with `CreateFileMappingW` / `MapViewOfFile` — see
//! `crate::mmap_win`), and every
//! edit appends new bytes to an append-only heap buffer. The visible document
//! is the ordered list of [`PieceNode`]s pointing into those two stores, so
//! inserts and deletes only move small node records, never file data.
//!
//! Lookup acceleration (SPEC.md section 2.2, roadmap item 2): the piece
//! list itself stays a `Vec<PieceNode>`, but a Fenwick (binary indexed)
//! tree over the piece lengths is maintained alongside it, so locating
//! the piece that contains a byte offset — and the offset a piece starts
//! at — costs O(log N) instead of a linear scan. Structural mutations
//! rebuild the tree in O(N), the same order as the `Vec` splice they
//! accompany; the hot typing path (coalescing extend) updates it in
//! O(log N). Neighbouring pieces that reference one backing store
//! contiguously are merged by [`PieceTable::coalesce`], which also runs
//! automatically once the table grows past
//! [`AUTO_COALESCE_THRESHOLD`] pieces.

use std::borrow::Cow;

/// Which backing store a [`PieceNode`] reads from (SPEC.md section 2.1).
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BufferSource {
    /// The original file, read-only.
    Original = 0,
    /// The append-only heap buffer holding edited-in bytes.
    Add = 1,
}

/// One contiguous run of bytes in the visible document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PieceNode {
    pub source: BufferSource,
    /// Byte offset of the run inside its backing store.
    pub source_offset: u64,
    /// Number of bytes in the run. Never zero in a live table.
    pub length: u64,
}

impl PieceNode {
    pub fn new(source: BufferSource, source_offset: u64, length: u64) -> Self {
        Self {
            source,
            source_offset,
            length,
        }
    }

    /// Exclusive end offset of this run inside its backing store.
    pub fn source_end(&self) -> u64 {
        self.source_offset + self.length
    }
}

/// Backing store for the original file bytes.
///
/// This enum is the seam where the Windows memory-mapped view slots in:
/// every read in [`PieceTable`] goes through it, so no piece-table API
/// changes were needed when the mmap variant landed (Slice 3).
///
/// Three variants exist:
///
/// * `Memory` — the original bytes are held in a `Vec<u8>`. Used by
///   [`PieceTable::from_original`], by tests, and by the headless tools.
/// * `LengthOnly` — only the original length is known and no byte storage
///   is attached (also the transient state inside a save while our own
///   mapping of the target is closed, SPEC.md section 5.1 step 4).
///   Structural operations (insert, delete, undo) behave normally;
///   reading an `Original` piece yields zero bytes of the correct
///   length. Byte-accurate reading is only promised for `Memory` and
///   `Mapped`.
/// * `Mapped` (Windows only) — the original bytes are a read-only
///   memory-mapped view of the file ([`crate::mmap_win::MmapFile`]),
///   the production backing of SPEC.md section 2.1: the OS pages bytes
///   in lazily and nothing is loaded into RAM up front.
#[derive(Debug)]
pub enum OriginalBuffer {
    Memory(Vec<u8>),
    LengthOnly {
        len: u64,
    },
    #[cfg(windows)]
    Mapped(std::sync::Arc<crate::mmap_win::MmapFile>),
}

impl OriginalBuffer {
    /// Length of the original byte source in bytes.
    pub fn len(&self) -> u64 {
        match self {
            OriginalBuffer::Memory(bytes) => bytes.len() as u64,
            OriginalBuffer::LengthOnly { len } => *len,
            #[cfg(windows)]
            OriginalBuffer::Mapped(mmap) => mmap.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Path of the mapped file when this backing is a Windows mmap;
    /// `None` for the memory and length-only backings. The save pipeline
    /// uses it to re-open a mapping it had to close for the atomic swap.
    pub fn mapped_path(&self) -> Option<&std::path::Path> {
        match self {
            #[cfg(windows)]
            OriginalBuffer::Mapped(mmap) => mmap.path(),
            _ => None,
        }
    }
}

/// Piece count above which an edit triggers an automatic
/// [`PieceTable::coalesce`] pass (roadmap item 2). Typing coalescence
/// keeps ordinary editing far below this; the pass exists for
/// pathological fragmentation (bulk replaces, many small pastes).
pub const AUTO_COALESCE_THRESHOLD: usize = 8192;

/// Build a Fenwick (binary indexed) tree over `lengths`: `tree[i]`
/// (1-based) holds the sum of the `lowbit(i)` lengths ending at piece
/// `i - 1`. Prefix sums and point updates are O(log N).
fn build_fenwick(lengths: &[u64]) -> Vec<u64> {
    let n = lengths.len();
    let mut tree = vec![0u64; n + 1];
    for (i, &len) in lengths.iter().enumerate() {
        tree[i + 1] = len;
    }
    for i in 1..=n {
        let j = i + (i & i.wrapping_neg());
        if j <= n {
            tree[j] += tree[i];
        }
    }
    tree
}

/// The piece table: an ordered list of pieces over the original file and the
/// append buffer (SPEC.md section 2.1).
#[derive(Debug)]
pub struct PieceTable {
    /// Size of the original file in bytes. Never changes after opening;
    /// edits live in `append_buffer` and the piece list.
    pub original_file_size: u64,
    /// Append-only heap buffer. Bytes are only ever pushed; undo never
    /// removes or rewrites bytes here (SPEC.md section 5.2).
    pub append_buffer: Vec<u8>,
    /// The visible document as an ordered list of pieces.
    pub pieces: Vec<PieceNode>,
    /// Total visible length in bytes: always the sum of all piece lengths.
    pub total_length: u64,
    /// Fenwick tree over the piece lengths (see [`build_fenwick`]);
    /// `weights.len() == pieces.len() + 1` whenever it is in sync.
    /// Every mutation path in this module keeps it in sync, and code
    /// outside the module that rewrites `pieces` directly (the undo
    /// engine's segment splice) calls [`PieceTable::rebuild_weights`].
    weights: Vec<u64>,
    /// How many times [`PieceTable::coalesce`] actually merged pieces.
    /// The undo engine watches this to detect a coalesce that happened
    /// inside one of its recorded edits (its segment bookkeeping then
    /// falls back to whole-layout entries — see `crate::undo`).
    coalesce_gen: u64,
    /// Piece count at which the next automatic coalesce pass may run
    /// (the throttle in `maybe_auto_coalesce`).
    next_auto_coalesce: usize,
    /// Backing store for `BufferSource::Original` pieces.
    original: OriginalBuffer,
}

impl PieceTable {
    /// Create a table for an original source of `original_len` bytes whose
    /// byte storage is not attached yet (see [`OriginalBuffer::LengthOnly`]).
    /// A non-empty source is represented by a single `Original` piece.
    pub fn new(original_len: u64) -> Self {
        let pieces = if original_len == 0 {
            Vec::new()
        } else {
            vec![PieceNode::new(BufferSource::Original, 0, original_len)]
        };
        let weights = build_fenwick(&pieces.iter().map(|p| p.length).collect::<Vec<_>>());
        Self {
            original_file_size: original_len,
            append_buffer: Vec::new(),
            pieces,
            total_length: original_len,
            weights,
            coalesce_gen: 0,
            next_auto_coalesce: AUTO_COALESCE_THRESHOLD + 1,
            original: OriginalBuffer::LengthOnly { len: original_len },
        }
    }

    /// Create a table whose original bytes are held in memory. Used by tests
    /// and headless tools; the production Windows path passes an mmap
    /// backing store through the same seam instead (see
    /// [`PieceTable::from_original_buffer`]).
    pub fn from_original(bytes: Vec<u8>) -> Self {
        Self::from_original_buffer(OriginalBuffer::Memory(bytes))
    }

    /// Create a table over any original-bytes backing store — the single
    /// constructor every backing (memory, length-only, Windows mmap)
    /// flows through. A non-empty source starts as a single `Original`
    /// piece covering the whole store.
    pub fn from_original_buffer(original: OriginalBuffer) -> Self {
        let len = original.len();
        let pieces = if len == 0 {
            Vec::new()
        } else {
            vec![PieceNode::new(BufferSource::Original, 0, len)]
        };
        let weights = build_fenwick(&pieces.iter().map(|p| p.length).collect::<Vec<_>>());
        Self {
            original_file_size: len,
            append_buffer: Vec::new(),
            pieces,
            total_length: len,
            weights,
            coalesce_gen: 0,
            next_auto_coalesce: AUTO_COALESCE_THRESHOLD + 1,
            original,
        }
    }

    /// Create a table whose original bytes are the given Windows
    /// memory-mapped file (SPEC.md section 2.1). No bytes are read at
    /// open time; the OS pages them in on first touch.
    #[cfg(windows)]
    pub fn from_mapped(mmap: std::sync::Arc<crate::mmap_win::MmapFile>) -> Self {
        Self::from_original_buffer(OriginalBuffer::Mapped(mmap))
    }

    /// Replace the original backing with a `LengthOnly` placeholder of
    /// the same length and return the old backing (dropping it when the
    /// caller drops the return value). The save pipeline uses this to
    /// close *our own* mapping of the target file before the atomic swap
    /// (SPEC.md section 5.1 step 4). Piece layout and totals are
    /// unchanged; until a new backing is installed, `Original` reads
    /// yield zero bytes (the `LengthOnly` contract above).
    pub(crate) fn detach_original(&mut self) -> OriginalBuffer {
        let len = self.original_file_size;
        std::mem::replace(&mut self.original, OriginalBuffer::LengthOnly { len })
    }

    /// Swap only the original backing store, leaving the piece layout,
    /// append buffer and totals exactly as they are. The save pipeline
    /// uses this to restore the pre-save backing after a *failed* swap
    /// (the document did not change, so the backing must not either).
    /// The new backing's length must equal `original_file_size`.
    pub(crate) fn replace_original(&mut self, backing: OriginalBuffer) {
        debug_assert_eq!(backing.len(), self.original_file_size);
        self.original = backing;
        debug_assert!(self.check_invariants());
    }

    /// Reset the whole table to a freshly-opened state over `new_original`:
    /// the append buffer is cleared and the document becomes a single
    /// `Original` piece (or no pieces, for an empty source). The atomic
    /// save pipeline calls this after a successful swap with the saved
    /// file's backing (SPEC.md section 5.1 step 6) — the "save is a
    /// history boundary" reset; the matching history clear lives in
    /// `crate::save::PieceTable::save_to`.
    pub(crate) fn reset_to_original(&mut self, new_original: OriginalBuffer) {
        let len = new_original.len();
        self.original = new_original;
        self.original_file_size = len;
        self.append_buffer.clear();
        self.pieces = if len == 0 {
            Vec::new()
        } else {
            vec![PieceNode::new(BufferSource::Original, 0, len)]
        };
        self.total_length = len;
        self.next_auto_coalesce = AUTO_COALESCE_THRESHOLD + 1;
        self.rebuild_weights();
        debug_assert!(self.check_invariants());
    }

    /// Total visible length in bytes.
    pub fn len(&self) -> u64 {
        self.total_length
    }

    pub fn is_empty(&self) -> bool {
        self.total_length == 0
    }

    /// Number of pieces currently describing the document.
    pub fn piece_count(&self) -> usize {
        self.pieces.len()
    }

    /// Rebuild the Fenwick weight tree from the current piece list.
    /// Called by every structural mutation in this module, and by the
    /// undo engine after it splices `pieces` directly.
    pub(crate) fn rebuild_weights(&mut self) {
        let lengths: Vec<u64> = self.pieces.iter().map(|p| p.length).collect();
        self.weights = build_fenwick(&lengths);
    }

    /// How many times [`PieceTable::coalesce`] merged pieces (the undo
    /// engine compares this around an edit it records).
    pub(crate) fn coalesce_gen(&self) -> u64 {
        self.coalesce_gen
    }

    /// Sum of the lengths of pieces `[0, idx)` — the document offset at
    /// which piece `idx` starts. O(log N) via the Fenwick tree.
    pub fn offset_of_piece(&self, idx: usize) -> u64 {
        debug_assert!(idx <= self.pieces.len());
        let mut sum: u64 = 0;
        let mut i = idx;
        while i > 0 {
            sum += self.weights[i];
            i -= i & i.wrapping_neg();
        }
        sum
    }

    /// Add `delta` to the length of piece `idx` in the Fenwick tree.
    fn weights_add(&mut self, idx: usize, delta: u64) {
        let mut i = idx + 1;
        while i < self.weights.len() {
            self.weights[i] += delta;
            i += i & i.wrapping_neg();
        }
    }

    /// The piece containing byte `offset`: its index and the document
    /// offset at which it starts. `None` when `offset` is at or past
    /// the end of the document. O(log N) via the Fenwick tree (binary
    /// lifting over the prefix sums).
    pub fn piece_at_offset(&self, offset: u64) -> Option<(usize, u64)> {
        if offset >= self.total_length || self.pieces.is_empty() {
            return None;
        }
        // Find the largest idx with prefix_sum(idx) <= offset; that idx
        // is the index of the piece containing `offset`. Standard
        // Fenwick binary lifting: walk bits from high to low, consuming
        // whole ranges whose sums fit in the remaining offset.
        let n = self.pieces.len();
        let mut idx: usize = 0;
        let mut remaining = offset;
        let mut bit: usize = 1;
        while bit << 1 <= n {
            bit <<= 1;
        }
        while bit > 0 {
            let next = idx + bit;
            if next <= n && self.weights[next] <= remaining {
                remaining -= self.weights[next];
                idx = next;
            }
            bit >>= 1;
        }
        debug_assert!(idx < n);
        let start = self.offset_of_piece(idx);
        debug_assert!(start <= offset && offset < start + self.pieces[idx].length);
        Some((idx, start))
    }

    /// Merge every pair of neighbouring pieces that reference the same
    /// backing store with contiguous ranges (piece B starts exactly
    /// where piece A ends, in the same store). Content is unchanged;
    /// only the node count drops. Returns how many merges happened.
    pub fn coalesce(&mut self) -> usize {
        if self.pieces.len() < 2 {
            return 0;
        }
        let mut merged: Vec<PieceNode> = Vec::with_capacity(self.pieces.len());
        let mut merges = 0usize;
        for piece in self.pieces.drain(..) {
            match merged.last_mut() {
                Some(last)
                    if last.source == piece.source && last.source_end() == piece.source_offset =>
                {
                    last.length += piece.length;
                    merges += 1;
                }
                _ => merged.push(piece),
            }
        }
        if merges > 0 {
            self.pieces = merged;
            self.coalesce_gen += 1;
            self.rebuild_weights();
            debug_assert!(self.check_invariants());
        } else {
            self.pieces = merged; // nothing merged; the list is unchanged
        }
        merges
    }

    /// The automatic coalescing trigger: after an edit, once the table
    /// exceeds [`AUTO_COALESCE_THRESHOLD`] pieces, run a full pass —
    /// throttled so a pass runs at most once per 1024 further pieces,
    /// keeping a long run of small inserts from paying an O(N) pass
    /// for every keystroke.
    fn maybe_auto_coalesce(&mut self) {
        if self.pieces.len() > AUTO_COALESCE_THRESHOLD
            && self.pieces.len() >= self.next_auto_coalesce
        {
            self.coalesce();
            self.next_auto_coalesce = self.pieces.len().max(AUTO_COALESCE_THRESHOLD) + 1024;
        }
    }

    /// Check the table invariants: piece lengths sum to `total_length`, no
    /// zero-length pieces exist, and every piece stays inside its backing
    /// store. The full gate asserts this after every operation in tests.
    pub fn check_invariants(&self) -> bool {
        let mut sum: u64 = 0;
        for piece in &self.pieces {
            if piece.length == 0 {
                return false;
            }
            let store_len = match piece.source {
                BufferSource::Original => self.original_file_size,
                BufferSource::Add => self.append_buffer.len() as u64,
            };
            if piece.source_end() > store_len {
                return false;
            }
            sum += piece.length;
        }
        // The Fenwick tree must agree with the piece list it indexes.
        let fenwick_ok = self.weights.len() == self.pieces.len() + 1
            && self.offset_of_piece(self.pieces.len()) == self.total_length;
        sum == self.total_length && self.original.len() == self.original_file_size && fenwick_ok
    }

    /// The byte at visible `offset`, or `None` past the end. The piece
    /// is located in O(log N) through the Fenwick weight index.
    pub fn byte_at(&self, offset: u64) -> Option<u8> {
        if offset >= self.total_length {
            return None;
        }
        let mut buf = [0u8; 1];
        let read = self.read_into(offset, &mut buf);
        if read == 1 {
            Some(buf[0])
        } else {
            None
        }
    }

    /// Read up to `len` bytes starting at visible `offset`. The result is
    /// clamped at the end of the document, so it may be shorter than `len`
    /// (or empty when `offset` is at or past the end).
    pub fn read(&self, offset: u64, len: u64) -> Vec<u8> {
        let mut out = vec![0u8; len as usize];
        let copied = self.read_into(offset, &mut out);
        out.truncate(copied);
        out
    }

    /// Copy document bytes starting at visible `offset` into `out`.
    /// Returns the number of bytes copied (clamped at the document end).
    pub fn read_into(&self, offset: u64, out: &mut [u8]) -> usize {
        if out.is_empty() || offset >= self.total_length {
            return 0;
        }
        let want = (out.len() as u64).min(self.total_length - offset);
        let (mut idx, mut piece_start) = match self.piece_at_offset(offset) {
            Some(found) => found,
            None => return 0,
        };
        let mut written: u64 = 0;
        while written < want {
            let piece = self.pieces[idx];
            let skip = offset + written - piece_start;
            let avail = piece.length - skip;
            let take = avail.min(want - written);
            let dst = &mut out[written as usize..(written + take) as usize];
            self.copy_piece_bytes(&piece, skip, dst);
            written += take;
            piece_start += piece.length;
            idx += 1;
        }
        written as usize
    }

    /// Iterate the document as one byte slice per piece, in order. This is
    /// the streaming path the atomic save pipeline (SPEC.md section 5.1) and
    /// the viewport slice generator (section 3.1) consume: callers copy each
    /// slice out in bounded chunks instead of materialising the document.
    pub fn iter_slices(&self) -> impl Iterator<Item = Cow<'_, [u8]>> + '_ {
        self.pieces.iter().map(|piece| self.piece_slice(piece))
    }

    fn piece_slice<'a>(&'a self, piece: &PieceNode) -> Cow<'a, [u8]> {
        let start = piece.source_offset as usize;
        let len = piece.length as usize;
        match piece.source {
            BufferSource::Add => Cow::Borrowed(&self.append_buffer[start..start + len]),
            BufferSource::Original => match &self.original {
                OriginalBuffer::Memory(bytes) => Cow::Borrowed(&bytes[start..start + len]),
                // No storage attached: yield correctly-sized zero bytes.
                OriginalBuffer::LengthOnly { .. } => Cow::Owned(vec![0u8; len]),
                #[cfg(windows)]
                OriginalBuffer::Mapped(mmap) => Cow::Borrowed(&mmap.as_slice()[start..start + len]),
            },
        }
    }

    /// Copy `dst.len()` bytes from `piece`, starting `skip` bytes into the
    /// piece. `dst` may be shorter than the rest of the piece (partial read).
    fn copy_piece_bytes(&self, piece: &PieceNode, skip: u64, dst: &mut [u8]) {
        debug_assert!((dst.len() as u64) <= piece.length - skip);
        let start = (piece.source_offset + skip) as usize;
        let len = dst.len();
        match piece.source {
            BufferSource::Add => dst.copy_from_slice(&self.append_buffer[start..start + len]),
            BufferSource::Original => match &self.original {
                OriginalBuffer::Memory(bytes) => dst.copy_from_slice(&bytes[start..start + len]),
                OriginalBuffer::LengthOnly { .. } => dst.fill(0),
                #[cfg(windows)]
                OriginalBuffer::Mapped(mmap) => {
                    dst.copy_from_slice(&mmap.as_slice()[start..start + len])
                }
            },
        }
    }

    /// Insert `bytes` at visible `offset`.
    ///
    /// The bytes are appended to the append buffer and a piece pointing at
    /// them is spliced into the piece list, bisecting the piece that contains
    /// `offset` when the insert lands inside one. Coalescence (SPEC.md
    /// section 2.2): when the insert lands exactly at the end of an `Add`
    /// piece whose data ends at the current end of the append buffer — the
    /// live-typing case — that piece is extended instead of adding a node.
    ///
    /// Panics if `offset` is past the end of the document.
    pub fn insert(&mut self, offset: u64, bytes: &[u8]) {
        assert!(
            offset <= self.total_length,
            "insert offset {offset} past end {}",
            self.total_length
        );
        if bytes.is_empty() {
            return;
        }
        let n = bytes.len() as u64;

        // Coalescence: the piece ending exactly at `offset` is the one
        // containing `offset - 1` (located in O(log N)). When it is an
        // `Add` piece whose data ends at the current end of the append
        // buffer — the live-typing case — extend it in place.
        if offset > 0 {
            if let Some((idx, start)) = self.piece_at_offset(offset - 1) {
                let piece = self.pieces[idx];
                if start + piece.length == offset
                    && piece.source == BufferSource::Add
                    && piece.source_end() == self.append_buffer.len() as u64
                {
                    self.append_buffer.extend_from_slice(bytes);
                    self.pieces[idx].length += n;
                    self.weights_add(idx, n);
                    self.total_length += n;
                    debug_assert!(self.check_invariants());
                    return;
                }
            }
        }

        let append_offset = self.append_buffer.len() as u64;
        self.append_buffer.extend_from_slice(bytes);
        let new_piece = PieceNode::new(BufferSource::Add, append_offset, n);

        if offset == self.total_length {
            self.pieces.push(new_piece);
        } else if offset == 0 {
            self.pieces.insert(0, new_piece);
        } else {
            // Locate the piece containing offset (O(log N)) and bisect
            // it, or insert at its start boundary.
            let (idx, start) = self
                .piece_at_offset(offset)
                .expect("offset < total_length has a piece");
            if offset == start {
                self.pieces.insert(idx, new_piece);
            } else {
                let piece = self.pieces[idx];
                let left_len = offset - start;
                let right = PieceNode::new(
                    piece.source,
                    piece.source_offset + left_len,
                    piece.length - left_len,
                );
                self.pieces[idx].length = left_len;
                self.pieces.insert(idx + 1, new_piece);
                self.pieces.insert(idx + 2, right);
            }
        }
        self.total_length += n;
        self.rebuild_weights();
        self.maybe_auto_coalesce();
        debug_assert!(self.check_invariants());
    }

    /// Delete `len` bytes starting at visible `offset`. `len` is clamped at
    /// the end of the document. Returns the removed content as pieces
    /// (clipped to the deleted range, in document order) — exactly what the
    /// undo engine stores as `EditAction::Delete::previous_pieces`
    /// (SPEC.md section 5.2). The underlying bytes are never touched: the
    /// append buffer keeps deleted `Add` bytes, and the original store is
    /// read-only.
    ///
    /// Panics if `offset` is past the end of the document.
    pub fn delete(&mut self, offset: u64, len: u64) -> Vec<PieceNode> {
        assert!(
            offset <= self.total_length,
            "delete offset {offset} past end {}",
            self.total_length
        );
        if len == 0 || offset == self.total_length {
            return Vec::new();
        }
        let del_end = offset + len.min(self.total_length - offset);
        let mut removed: Vec<PieceNode> = Vec::new();
        let mut new_pieces: Vec<PieceNode> = Vec::with_capacity(self.pieces.len());
        // Pieces entirely before the deleted range are kept untouched;
        // jump straight to the first overlapping piece in O(log N) and
        // prefill the kept prefix.
        let (first_idx, mut piece_start) = match self.piece_at_offset(offset) {
            Some(found) => found,
            None => (self.pieces.len(), self.total_length),
        };
        new_pieces.extend_from_slice(&self.pieces[..first_idx]);
        for piece in &self.pieces[first_idx..] {
            let piece_end = piece_start + piece.length;
            if piece_end <= offset || piece_start >= del_end {
                // No overlap: keep whole piece.
                new_pieces.push(*piece);
            } else {
                // Overlap: record the removed sub-range as a clipped piece.
                let rem_start = offset.max(piece_start);
                let rem_end = del_end.min(piece_end);
                removed.push(PieceNode::new(
                    piece.source,
                    piece.source_offset + (rem_start - piece_start),
                    rem_end - rem_start,
                ));
                // Keep a left remnant and/or a right remnant.
                if piece_start < offset {
                    new_pieces.push(PieceNode::new(
                        piece.source,
                        piece.source_offset,
                        offset - piece_start,
                    ));
                }
                if piece_end > del_end {
                    new_pieces.push(PieceNode::new(
                        piece.source,
                        piece.source_offset + (del_end - piece_start),
                        piece_end - del_end,
                    ));
                }
            }
            piece_start = piece_end;
        }
        let removed_len: u64 = removed.iter().map(|p| p.length).sum();
        self.pieces = new_pieces;
        self.total_length -= removed_len;
        self.rebuild_weights();
        self.maybe_auto_coalesce();
        debug_assert!(self.check_invariants());
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(bytes: &[u8]) -> PieceTable {
        PieceTable::from_original(bytes.to_vec())
    }

    #[test]
    fn empty_table_has_no_pieces() {
        let table = PieceTable::new(0);
        assert_eq!(table.piece_count(), 0);
        assert_eq!(table.len(), 0);
        assert!(table.is_empty());
        assert_eq!(table.read(0, 10), Vec::<u8>::new());
        assert_eq!(table.byte_at(0), None);
        assert!(table.check_invariants());
    }

    #[test]
    fn from_original_is_one_piece() {
        let table = doc(b"hello world");
        assert_eq!(table.piece_count(), 1);
        assert_eq!(table.len(), 11);
        assert_eq!(table.read(0, 11), b"hello world");
        assert!(table.check_invariants());
    }

    #[test]
    fn insert_at_start_middle_and_end() {
        let mut table = doc(b"ac");
        table.insert(1, b"b");
        assert_eq!(table.read(0, 99), b"abc");
        table.insert(0, b">");
        assert_eq!(table.read(0, 99), b">abc");
        table.insert(4, b"<");
        assert_eq!(table.read(0, 99), b">abc<");
        assert!(table.check_invariants());
    }

    #[test]
    fn insert_into_empty_table() {
        let mut table = PieceTable::new(0);
        table.insert(0, b"first");
        assert_eq!(table.read(0, 99), b"first");
        assert_eq!(table.piece_count(), 1);
        assert!(table.check_invariants());
    }

    #[test]
    fn read_across_piece_boundaries() {
        let mut table = doc(b"0123456789");
        table.insert(5, b"ABCDE");
        // Document is now 01234 ABCDE 56789 spread over three pieces.
        assert_eq!(table.piece_count(), 3);
        assert_eq!(table.read(3, 9), b"34ABCDE56");
        assert_eq!(table.read(0, 15), b"01234ABCDE56789");
        let mut buf = [0u8; 4];
        assert_eq!(table.read_into(9, &mut buf), 4);
        assert_eq!(&buf, b"E567");
        // Reads clamp at the end.
        assert_eq!(table.read(13, 50), b"89");
        assert_eq!(table.read(15, 5), Vec::<u8>::new());
        assert!(table.check_invariants());
    }

    #[test]
    fn byte_at_reads_each_store() {
        let mut table = doc(b"xy");
        table.insert(1, b"Z");
        assert_eq!(table.byte_at(0), Some(b'x'));
        assert_eq!(table.byte_at(1), Some(b'Z'));
        assert_eq!(table.byte_at(2), Some(b'y'));
        assert_eq!(table.byte_at(3), None);
    }

    #[test]
    fn iter_slices_reassembles_document() {
        let mut table = doc(b"hello");
        table.insert(5, b" ");
        table.insert(6, b"world");
        let joined: Vec<u8> = table
            .iter_slices()
            .flat_map(|slice| slice.into_owned())
            .collect();
        assert_eq!(joined, b"hello world");
    }

    #[test]
    fn sequential_typing_coalesces_into_one_add_piece() {
        let mut table = doc(b"");
        table.insert(0, b"h");
        table.insert(1, b"e");
        table.insert(2, b"l");
        table.insert(3, b"l");
        table.insert(4, b"o");
        // Five inserts, one piece: the section 2.2 coalescence rule.
        assert_eq!(table.piece_count(), 1);
        assert_eq!(table.pieces[0].source, BufferSource::Add);
        assert_eq!(table.pieces[0].length, 5);
        assert_eq!(table.read(0, 99), b"hello");
        assert!(table.check_invariants());
    }

    #[test]
    fn coalescence_does_not_merge_non_contiguous_inserts() {
        let mut table = doc(b"ab");
        table.insert(2, b"X"); // Add piece at end of document.
        table.insert(0, b"Y"); // Insert at start: not contiguous with X's piece end.
        assert_eq!(table.read(0, 99), b"YabX");
        // Y must be its own piece, not merged into X.
        let add_pieces: Vec<&PieceNode> = table
            .pieces
            .iter()
            .filter(|p| p.source == BufferSource::Add)
            .collect();
        assert_eq!(add_pieces.len(), 2);
        assert!(table.check_invariants());
    }

    #[test]
    fn delete_inside_one_piece_splits_it() {
        let mut table = doc(b"hello world");
        let removed = table.delete(5, 1);
        assert_eq!(table.read(0, 99), b"helloworld");
        assert_eq!(table.piece_count(), 2);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].source, BufferSource::Original);
        assert_eq!(removed[0].source_offset, 5);
        assert_eq!(removed[0].length, 1);
        assert!(table.check_invariants());
    }

    #[test]
    fn delete_across_pieces_returns_clipped_pieces() {
        let mut table = doc(b"AAAA");
        table.insert(2, b"BB");
        // Document: AA BB AA over three pieces. Delete "A BB A" (offsets 1..5).
        let removed = table.delete(1, 4);
        assert_eq!(table.read(0, 99), b"AA");
        assert_eq!(removed.len(), 3);
        assert_eq!(removed[0], PieceNode::new(BufferSource::Original, 1, 1));
        assert_eq!(removed[1], PieceNode::new(BufferSource::Add, 0, 2));
        assert_eq!(removed[2], PieceNode::new(BufferSource::Original, 2, 1));
        assert!(table.check_invariants());
    }

    #[test]
    fn delete_everything_and_delete_clamps() {
        let mut table = doc(b"short");
        let removed = table.delete(3, 1000);
        assert_eq!(removed.iter().map(|p| p.length).sum::<u64>(), 2);
        assert_eq!(table.read(0, 99), b"sho");
        let removed = table.delete(0, 3);
        assert_eq!(removed.iter().map(|p| p.length).sum::<u64>(), 3);
        assert_eq!(table.len(), 0);
        assert_eq!(table.piece_count(), 0);
        assert!(table.check_invariants());
    }

    #[test]
    fn length_only_table_supports_structural_edits() {
        let mut table = PieceTable::new(100);
        assert_eq!(table.piece_count(), 1);
        table.insert(50, b"new");
        assert_eq!(table.len(), 103);
        assert_eq!(table.piece_count(), 3);
        let removed = table.delete(0, 10);
        assert_eq!(removed.iter().map(|p| p.length).sum::<u64>(), 10);
        assert_eq!(table.len(), 93);
        assert!(table.check_invariants());
    }

    /// Tiny deterministic PRNG (xorshift64) for the randomised tests —
    /// no dependency, same script every run.
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: u64) -> u64 {
            if n == 0 {
                0
            } else {
                self.next() % n
            }
        }
    }

    #[test]
    fn random_edit_scripts_match_a_naive_model() {
        // Roadmap item 2 equivalence: the Fenwick-accelerated table
        // must produce byte-identical content and identical piece
        // lookups to the plain semantics, across random edit scripts.
        let mut rng = XorShift(0x1234_5678_9ABC_DEF1);
        let mut table = doc(b"seed document\nwith two lines\n");
        let mut model: Vec<u8> = table.read(0, table.len());
        for step in 0..600 {
            if model.is_empty() || rng.below(2) == 0 {
                let at = rng.below(model.len() as u64 + 1);
                let bytes: Vec<u8> = (0..rng.below(9))
                    .map(|_| b"abcXYZ 09\n"[rng.below(10) as usize])
                    .collect();
                table.insert(at, &bytes);
                model.splice(at as usize..at as usize, bytes.iter().copied());
            } else {
                let at = rng.below(model.len() as u64);
                let len = rng.below(12);
                table.delete(at, len);
                let end = (at + len).min(model.len() as u64) as usize;
                model.drain(at as usize..end);
            }
            assert_eq!(table.read(0, table.len()), model, "content at step {step}");
            assert!(table.check_invariants(), "invariants at step {step}");
            // Lookups agree with the content: every offset's piece
            // contains it, and piece starts are the prefix sums.
            for _probe in 0..8 {
                let off = rng.below(model.len() as u64 + 1);
                if off < model.len() as u64 {
                    let (idx, start) = table.piece_at_offset(off).unwrap();
                    assert!(start <= off && off < start + table.pieces[idx].length);
                    assert_eq!(table.offset_of_piece(idx), start);
                    assert_eq!(table.byte_at(off), Some(model[off as usize]));
                } else {
                    assert_eq!(table.piece_at_offset(off), None);
                }
            }
            assert_eq!(
                table.offset_of_piece(table.piece_count()),
                table.len(),
                "piece starts sum to the document length at step {step}"
            );
        }
    }

    #[test]
    fn hundred_thousand_pieces_answer_lookups_at_boundaries() {
        // Build the fragmented table directly (100k one-byte inserts
        // would pay the per-insert debug invariant check 100k times):
        // piece k holds append-buffer byte n-1-k, so the document is
        // the reverse of the buffer — and no two neighbours are
        // contiguous in the store, so nothing can coalesce.
        let n = 100_000u64;
        let mut table = PieceTable::new(0);
        table.append_buffer = (0..n).map(|i| (i % 251) as u8).collect();
        table.pieces = (0..n)
            .map(|k| PieceNode::new(BufferSource::Add, n - 1 - k, 1))
            .collect();
        table.total_length = n;
        table.rebuild_weights();
        assert_eq!(table.piece_count(), n as usize);
        assert!(table.check_invariants());
        // Byte at document offset k is append-buffer byte n-1-k.
        for k in [0u64, 1, 2, 999, 50_000, 99_998, 99_999] {
            let expect = ((n - 1 - k) % 251) as u8;
            assert_eq!(table.byte_at(k), Some(expect), "byte at {k}");
            let (idx, start) = table.piece_at_offset(k).unwrap();
            assert_eq!((idx, start), (k as usize, k), "one-byte pieces at {k}");
        }
        assert_eq!(table.read(n - 3, 3), vec![2, 1, 0]);
        assert_eq!(table.piece_at_offset(n), None);
        // A real edit at this scale still lands correctly (and the
        // automatic coalesce pass merges nothing here: neighbours are
        // not contiguous in the append buffer).
        table.insert(50_000, b"!");
        assert_eq!(table.byte_at(50_000), Some(b'!'));
        assert_eq!(table.read(49_999, 3)[0], ((n - 1 - 49_999) % 251) as u8);
        assert!(table.check_invariants());
    }

    #[test]
    fn coalesce_merges_contiguous_neighbours_without_changing_content() {
        // "abcdef" with "ZZ" inserted in the middle: three pieces.
        let mut table = doc(b"abcdef");
        table.insert(2, b"ZZ");
        assert_eq!(table.piece_count(), 3);
        // Deleting the inserted piece leaves two Original pieces that
        // are contiguous in the original store (0..2 and 2..6).
        table.delete(2, 2);
        assert_eq!(table.piece_count(), 2);
        assert_eq!(table.read(0, 99), b"abcdef");
        assert_eq!(table.coalesce(), 1);
        assert_eq!(table.piece_count(), 1);
        assert_eq!(
            table.pieces[0],
            PieceNode::new(BufferSource::Original, 0, 6)
        );
        assert_eq!(table.read(0, 99), b"abcdef");
        // A second pass is a no-op.
        assert_eq!(table.coalesce(), 0);
        assert!(table.check_invariants());
    }

    #[test]
    fn many_mixed_edits_keep_invariants_and_content() {
        let mut table = doc(b"The quick brown fox");
        table.insert(4, b"very ");
        table.delete(15, 6); // removes "brown " after the insert shifted it
        table.insert(0, b">> ");
        table.insert(table.len(), b" <<");
        assert!(table.check_invariants());
        let text = String::from_utf8(table.read(0, table.len())).unwrap();
        assert_eq!(text, ">> The very quick fox <<");
    }
}
