//! Memory-mapped piece table engine (SPEC.md section 2).
//!
//! The document is never held as one contiguous edited buffer. The original
//! file bytes stay in a read-only backing store (on Windows that will be an
//! mmap view created with `CreateFileMappingW` / `MapViewOfFile`), and every
//! edit appends new bytes to an append-only heap buffer. The visible document
//! is the ordered list of [`PieceNode`]s pointing into those two stores, so
//! inserts and deletes only move small node records, never file data.
//!
//! Production target (SPEC.md section 2.1): the `Vec<PieceNode>` below is
//! replaced by a weight-balanced 2-3 B-tree that maintains subtree lengths
//! for O(log N) lookups (section 2.2). All public operations in this module
//! are written against piece *semantics*, not Vec internals, so the B-tree
//! can slot in behind the same API in a later slice.

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
/// This enum is the seam where the Windows memory-mapped view slots in: the
/// next Windows slice adds a variant holding the mapped view, and every read
/// in [`PieceTable`] already goes through it, so no piece-table API changes.
///
/// Two variants exist in this foundation slice:
///
/// * `Memory` — the original bytes are held in a `Vec<u8>`. Used by
///   [`PieceTable::from_original`], by tests, and by the headless tools.
/// * `LengthOnly` — only the original length is known and no byte storage is
///   attached yet (the state a table is in right after a file is mapped but
///   before any view exists). Structural operations (insert, delete, undo)
///   behave normally; reading an `Original` piece yields zero bytes of the
///   correct length. Byte-accurate reading is only promised for `Memory`
///   (and, later, for the mmap variant).
#[derive(Debug)]
pub enum OriginalBuffer {
    Memory(Vec<u8>),
    LengthOnly { len: u64 },
}

impl OriginalBuffer {
    /// Length of the original byte source in bytes.
    pub fn len(&self) -> u64 {
        match self {
            OriginalBuffer::Memory(bytes) => bytes.len() as u64,
            OriginalBuffer::LengthOnly { len } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
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
        Self {
            original_file_size: original_len,
            append_buffer: Vec::new(),
            pieces,
            total_length: original_len,
            original: OriginalBuffer::LengthOnly { len: original_len },
        }
    }

    /// Create a table whose original bytes are held in memory. Used by tests
    /// and headless tools; the production Windows path will pass an mmap
    /// backing store through the same seam instead.
    pub fn from_original(bytes: Vec<u8>) -> Self {
        let len = bytes.len() as u64;
        let pieces = if len == 0 {
            Vec::new()
        } else {
            vec![PieceNode::new(BufferSource::Original, 0, len)]
        };
        Self {
            original_file_size: len,
            append_buffer: Vec::new(),
            pieces,
            total_length: len,
            original: OriginalBuffer::Memory(bytes),
        }
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
        sum == self.total_length && self.original.len() == self.original_file_size
    }

    /// The byte at visible `offset`, or `None` past the end. O(N) in the
    /// number of pieces in this Vec-based slice; O(log N) once the B-tree
    /// index from SPEC.md section 2.2 lands.
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
        let mut written: u64 = 0;
        let mut piece_start: u64 = 0;
        for piece in &self.pieces {
            if written >= want {
                break;
            }
            let piece_end = piece_start + piece.length;
            if piece_end > offset {
                let skip = offset.saturating_sub(piece_start);
                let avail = piece.length - skip;
                let take = avail.min(want - written);
                let dst = &mut out[written as usize..(written + take) as usize];
                self.copy_piece_bytes(piece, skip, dst);
                written += take;
            }
            piece_start = piece_end;
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

        // Coalescence: find the piece ending exactly at `offset`.
        let mut piece_start: u64 = 0;
        for (idx, piece) in self.pieces.iter().enumerate() {
            let piece_end = piece_start + piece.length;
            if piece_end == offset
                && piece.source == BufferSource::Add
                && piece.source_end() == self.append_buffer.len() as u64
            {
                self.append_buffer.extend_from_slice(bytes);
                self.pieces[idx].length += n;
                self.total_length += n;
                debug_assert!(self.check_invariants());
                return;
            }
            piece_start = piece_end;
        }

        let append_offset = self.append_buffer.len() as u64;
        self.append_buffer.extend_from_slice(bytes);
        let new_piece = PieceNode::new(BufferSource::Add, append_offset, n);

        if offset == self.total_length {
            self.pieces.push(new_piece);
        } else if offset == 0 {
            self.pieces.insert(0, new_piece);
        } else {
            // Find the piece containing offset and bisect it.
            let mut start: u64 = 0;
            let mut idx = 0;
            loop {
                let piece = self.pieces[idx];
                let end = start + piece.length;
                if offset == start {
                    self.pieces.insert(idx, new_piece);
                    break;
                }
                if offset > start && offset < end {
                    let left_len = offset - start;
                    let right = PieceNode::new(
                        piece.source,
                        piece.source_offset + left_len,
                        piece.length - left_len,
                    );
                    self.pieces[idx].length = left_len;
                    self.pieces.insert(idx + 1, new_piece);
                    self.pieces.insert(idx + 2, right);
                    break;
                }
                start = end;
                idx += 1;
            }
        }
        self.total_length += n;
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
        let mut piece_start: u64 = 0;
        for piece in &self.pieces {
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
