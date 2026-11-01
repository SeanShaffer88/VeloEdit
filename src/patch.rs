//! Delta patch core (roadmap R4): export the difference between a
//! document's original bytes and its edited piece table as a
//! compact binary patch, and apply such a patch to the original
//! bytes to reproduce the document exactly. Unchanged spans are
//! referenced by offset, never copied, so a small edit to a huge
//! file produces a tiny patch.
//!
//! # The `VELOPAT1` format
//!
//! All integers are little-endian.
//!
//! ```text
//! offset  size  field
//! 0       8     magic: the ASCII bytes "VELOPAT1"
//! 8       8     original length (u64)
//! 16      8     result length (u64) — the length applying produces
//! 24      4     hunk count (u32)
//! 28      ...   hunks, in ascending original-offset order
//! ```
//!
//! Each hunk:
//!
//! ```text
//! +0      8     original offset (u64) — where the change starts
//! +8      8     original length (u64) — original bytes replaced (0 = pure insert)
//! +16     8     replacement length (u64)
//! +24     N     the replacement bytes (0 = pure deletion)
//! ```
//!
//! The original bytes *between* hunks are unchanged and are copied
//! from the original when applying. Hunks never overlap, and the
//! declared result length must equal what applying produces — both
//! are checked on parse, so a corrupt patch is refused, never
//! half-applied.

use std::io::{self, Write};

use crate::piece_table::{BufferSource, PieceTable};
use crate::save::DEFAULT_MAX_CHUNK;

/// The format's magic header: `VELOPAT1`.
pub const PATCH_MAGIC: &[u8; 8] = b"VELOPAT1";

/// One change: the original bytes
/// `[original_offset, original_offset + original_length)` are
/// replaced by `replacement` (empty for a pure deletion; a zero
/// `original_length` is a pure insertion).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatchHunk {
    pub original_offset: u64,
    pub original_length: u64,
    pub replacement: Vec<u8>,
}

/// A whole patch: the original's length, the length applying
/// produces, and the hunks in ascending order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Patch {
    pub original_len: u64,
    pub result_len: u64,
    pub hunks: Vec<PatchHunk>,
}

/// Why a patch byte stream was refused, or applying failed.
#[derive(Debug)]
pub enum PatchError {
    /// The magic header is missing or wrong.
    BadMagic,
    /// The byte stream ends before the header or a hunk is
    /// complete.
    Truncated,
    /// Extra bytes follow the last hunk.
    TrailingBytes,
    /// Hunks overlap, are out of order, or reach past the original.
    InvalidHunks,
    /// The declared result length disagrees with the lengths the
    /// hunks imply.
    ResultLengthMismatch,
    /// The original supplied at apply time has a different length
    /// than the patch was built for.
    OriginalLengthMismatch { patch: u64, actual: u64 },
    /// Reading the edited document while building failed to deliver
    /// bytes a piece claims to hold (a broken table invariant).
    BrokenTable,
    /// Writing the applied output failed.
    Io(io::Error),
}

impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PatchError::BadMagic => write!(f, "not a VELOPAT1 patch (bad magic header)"),
            PatchError::Truncated => write!(f, "the patch is truncated"),
            PatchError::TrailingBytes => {
                write!(f, "extra bytes follow the patch's last hunk")
            }
            PatchError::InvalidHunks => write!(
                f,
                "the patch's hunks overlap, are out of order, or pass the original's end"
            ),
            PatchError::ResultLengthMismatch => write!(
                f,
                "the patch's declared result length disagrees with its hunks"
            ),
            PatchError::OriginalLengthMismatch { patch, actual } => write!(
                f,
                "the patch was built for a {patch}-byte original but the original has {actual} bytes"
            ),
            PatchError::BrokenTable => {
                write!(f, "the document's pieces did not deliver the bytes they describe")
            }
            PatchError::Io(e) => write!(f, "writing the patched output failed: {e}"),
        }
    }
}

impl std::error::Error for PatchError {}

impl From<io::Error> for PatchError {
    fn from(e: io::Error) -> Self {
        PatchError::Io(e)
    }
}

/// Build the patch that turns the original byte source into the
/// document `table` currently describes.
///
/// The document is walked piece by piece against the original:
/// contiguous original pieces that line up with the walk are
/// unchanged spans; everything else (inserted bytes, moved or
/// duplicated original bytes, deletions) folds into hunks.
/// `read_original(offset, buf) -> usize` reads the *original* bytes
/// (following the usual read contract) — it is needed to embed
/// original bytes the document moved to a new position, which a
/// patch cannot reference by offset. Zero-run pieces embed as
/// literal zero bytes (documented materialisation: the patch is a
/// byte stream, so a zero run becomes its zeros — sized by the run,
/// never by the document).
pub fn build_patch(
    original_len: u64,
    mut read_original: impl FnMut(u64, &mut [u8]) -> usize,
    table: &PieceTable,
) -> Result<Patch, PatchError> {
    let mut hunks: Vec<PatchHunk> = Vec::new();
    // The hunk currently being accumulated, if any:
    // (original offset, original length, replacement bytes).
    let mut open: Option<(u64, u64, Vec<u8>)> = None;
    // How much of the original the walk has accounted for.
    let mut cursor: u64 = 0;

    let mut close_hunk = |open: &mut Option<(u64, u64, Vec<u8>)>| {
        if let Some((offset, length, replacement)) = open.take() {
            // A zero-length, zero-replacement hunk carries nothing.
            if length > 0 || !replacement.is_empty() {
                hunks.push(PatchHunk {
                    original_offset: offset,
                    original_length: length,
                    replacement,
                });
            }
        }
    };
    let open_hunk = |open: &mut Option<(u64, u64, Vec<u8>)>, at: u64| {
        if open.is_none() {
            *open = Some((at, 0, Vec::new()));
        }
    };

    for piece in &table.pieces {
        match piece.source {
            BufferSource::Original => {
                let start = piece.source_offset;
                let len = piece.length;
                if start >= cursor {
                    if start > cursor {
                        // A gap: original bytes [cursor, start) are
                        // not in the document (deleted, or skipped
                        // and re-referenced later).
                        open_hunk(&mut open, cursor);
                        let h = open.as_mut().expect("hunk open");
                        h.1 += start - cursor;
                    }
                    // [cursor, start+len) is unchanged — unless a
                    // hunk is open, in which case this span follows
                    // replacement bytes and must close it first.
                    close_hunk(&mut open);
                    cursor = start + len;
                } else {
                    // Moved/duplicated original bytes: they appear
                    // in the document at a position that does not
                    // line up with the walk, so embed their bytes.
                    let bytes = read_original_exact(&mut read_original, start, len)?;
                    open_hunk(&mut open, cursor);
                    open.as_mut()
                        .expect("hunk open")
                        .2
                        .extend_from_slice(&bytes);
                }
            }
            BufferSource::Add => {
                let bytes = table
                    .append_buffer
                    .get(
                        piece.source_offset as usize..(piece.source_offset + piece.length) as usize,
                    )
                    .ok_or(PatchError::BrokenTable)?;
                open_hunk(&mut open, cursor);
                open.as_mut().expect("hunk open").2.extend_from_slice(bytes);
            }
            BufferSource::Zeroes => {
                open_hunk(&mut open, cursor);
                let h = open.as_mut().expect("hunk open");
                h.2.resize(h.2.len() + piece.length as usize, 0);
            }
        }
    }
    // Original bytes past the walk were deleted.
    if cursor < original_len {
        open_hunk(&mut open, cursor);
        open.as_mut().expect("hunk open").1 += original_len - cursor;
    }
    close_hunk(&mut open);

    Ok(Patch {
        original_len,
        result_len: table.len(),
        hunks,
    })
}

/// Read exactly `len` original bytes at `offset` (for embedding
/// moved bytes); a short read means the caller's source disagrees
/// with the table — a broken state, refused.
fn read_original_exact(
    read_original: &mut impl FnMut(u64, &mut [u8]) -> usize,
    offset: u64,
    len: u64,
) -> Result<Vec<u8>, PatchError> {
    let mut bytes = vec![0u8; len as usize];
    let mut filled = 0usize;
    while (filled as u64) < len {
        let read = read_original(offset + filled as u64, &mut bytes[filled..]);
        if read == 0 {
            return Err(PatchError::BrokenTable);
        }
        filled += read;
    }
    Ok(bytes)
}

impl Patch {
    /// Serialise to the `VELOPAT1` byte format (module docs).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            28 + self
                .hunks
                .iter()
                .map(|h| 24 + h.replacement.len())
                .sum::<usize>(),
        );
        out.extend_from_slice(PATCH_MAGIC);
        out.extend_from_slice(&self.original_len.to_le_bytes());
        out.extend_from_slice(&self.result_len.to_le_bytes());
        out.extend_from_slice(&(self.hunks.len() as u32).to_le_bytes());
        for hunk in &self.hunks {
            out.extend_from_slice(&hunk.original_offset.to_le_bytes());
            out.extend_from_slice(&hunk.original_length.to_le_bytes());
            out.extend_from_slice(&(hunk.replacement.len() as u64).to_le_bytes());
            out.extend_from_slice(&hunk.replacement);
        }
        out
    }

    /// Parse a `VELOPAT1` byte stream, validating the structure
    /// fully (magic, lengths, hunk ordering and bounds, the result
    /// length cross-check) — a patch that fails any check is
    /// refused whole.
    pub fn from_bytes(bytes: &[u8]) -> Result<Patch, PatchError> {
        let mut cursor = ByteCursor { bytes, pos: 0 };
        if cursor.take(8)? != PATCH_MAGIC.as_slice() {
            return Err(PatchError::BadMagic);
        }
        let original_len = cursor.u64()?;
        let result_len = cursor.u64()?;
        let count = cursor.u32()? as usize;
        let mut hunks = Vec::with_capacity(count.min(1 << 20));
        let mut prev_end: u64 = 0;
        let mut replaced: u64 = 0;
        let mut inserted: u64 = 0;
        for _ in 0..count {
            let original_offset = cursor.u64()?;
            let original_length = cursor.u64()?;
            let replacement_len = cursor.u64()?;
            let end_within = match original_offset.checked_add(original_length) {
                Some(end) => end <= original_len,
                None => false,
            };
            if original_offset < prev_end || !end_within {
                return Err(PatchError::InvalidHunks);
            }
            let replacement = cursor.take(replacement_len as usize)?.to_vec();
            prev_end = original_offset + original_length;
            replaced = replaced
                .checked_add(original_length)
                .ok_or(PatchError::InvalidHunks)?;
            inserted = inserted
                .checked_add(replacement_len)
                .ok_or(PatchError::InvalidHunks)?;
            hunks.push(PatchHunk {
                original_offset,
                original_length,
                replacement,
            });
        }
        if cursor.pos != bytes.len() {
            return Err(PatchError::TrailingBytes);
        }
        let implied = original_len
            .checked_sub(replaced)
            .and_then(|kept| kept.checked_add(inserted));
        if implied != Some(result_len) {
            return Err(PatchError::ResultLengthMismatch);
        }
        Ok(Patch {
            original_len,
            result_len,
            hunks,
        })
    }
}

/// A forward-only reader over the patch bytes.
struct ByteCursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> ByteCursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], PatchError> {
        let end = self.pos.checked_add(n).ok_or(PatchError::Truncated)?;
        let slice = self.bytes.get(self.pos..end).ok_or(PatchError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn u64(&mut self) -> Result<u64, PatchError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes(b.try_into().expect("8 bytes")))
    }

    fn u32(&mut self) -> Result<u32, PatchError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes(b.try_into().expect("4 bytes")))
    }
}

/// Apply `patch` to the original byte source, streaming the result
/// to `out`: unchanged spans are copied from the original in
/// bounded chunks, hunks write their replacement bytes. Returns the
/// number of bytes written (the patch's declared result length).
/// `read_at(offset, buf) -> usize` follows the usual read contract
/// over the original.
pub fn apply_patch(
    original_len: u64,
    mut read_at: impl FnMut(u64, &mut [u8]) -> usize,
    patch: &Patch,
    out: &mut impl Write,
) -> Result<u64, PatchError> {
    if original_len != patch.original_len {
        return Err(PatchError::OriginalLengthMismatch {
            patch: patch.original_len,
            actual: original_len,
        });
    }
    let mut written: u64 = 0;
    let mut cursor: u64 = 0; // original offset consumed so far
    let mut buffer = vec![0u8; DEFAULT_MAX_CHUNK];
    for hunk in &patch.hunks {
        // Unchanged span before this hunk.
        written += copy_span(
            &mut read_at,
            cursor,
            hunk.original_offset - cursor,
            &mut buffer,
            out,
        )?;
        out.write_all(&hunk.replacement)?;
        written += hunk.replacement.len() as u64;
        cursor = hunk.original_offset + hunk.original_length;
    }
    // Unchanged tail.
    written += copy_span(
        &mut read_at,
        cursor,
        original_len - cursor,
        &mut buffer,
        out,
    )?;
    Ok(written)
}

/// Stream `len` bytes of the original from `offset` to `out`.
fn copy_span(
    read_at: &mut impl FnMut(u64, &mut [u8]) -> usize,
    mut offset: u64,
    mut len: u64,
    buffer: &mut [u8],
    out: &mut impl Write,
) -> Result<u64, PatchError> {
    let mut written = 0u64;
    while len > 0 {
        let want = (len as usize).min(buffer.len());
        let read = read_at(offset, &mut buffer[..want]);
        if read == 0 {
            return Err(PatchError::BrokenTable);
        }
        out.write_all(&buffer[..read])?;
        offset += read as u64;
        len -= read as u64;
        written += read as u64;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader(data: &[u8]) -> impl FnMut(u64, &mut [u8]) -> usize + '_ {
        move |offset: u64, out: &mut [u8]| {
            let s = offset as usize;
            if s >= data.len() {
                return 0;
            }
            let n = out.len().min(data.len() - s);
            out[..n].copy_from_slice(&data[s..s + n]);
            n
        }
    }

    fn materialise(table: &PieceTable) -> Vec<u8> {
        let mut all = Vec::new();
        for slice in table.iter_slices() {
            all.extend_from_slice(&slice);
        }
        all
    }

    /// Build → serialise → parse → apply; the result must equal
    /// the document the table describes, byte for byte.
    fn round_trip(original: &[u8], table: &PieceTable) {
        let patch = build_patch(original.len() as u64, reader(original), table).unwrap();
        assert_eq!(patch.result_len, table.len());
        let bytes = patch.to_bytes();
        let parsed = Patch::from_bytes(&bytes).unwrap();
        assert_eq!(parsed, patch);
        let mut out: Vec<u8> = Vec::new();
        let written =
            apply_patch(original.len() as u64, reader(original), &parsed, &mut out).unwrap();
        assert_eq!(written, table.len());
        assert_eq!(out, materialise(table));
    }

    #[test]
    fn no_change_is_an_empty_patch() {
        let original = b"hello world, unchanged".to_vec();
        let table = PieceTable::from_original(original.clone());
        let patch = build_patch(original.len() as u64, reader(&original), &table).unwrap();
        assert!(patch.hunks.is_empty());
        assert_eq!(patch.result_len, original.len() as u64);
        round_trip(&original, &table);
    }

    #[test]
    fn insert_at_start_is_one_hunk() {
        let original = b"world".to_vec();
        let mut table = PieceTable::from_original(original.clone());
        table.insert(0, b"hello ");
        let patch = build_patch(5, reader(&original), &table).unwrap();
        assert_eq!(patch.hunks.len(), 1);
        assert_eq!(patch.hunks[0].original_offset, 0);
        assert_eq!(patch.hunks[0].original_length, 0);
        assert_eq!(patch.hunks[0].replacement, b"hello ");
        round_trip(&original, &table);
    }

    #[test]
    fn delete_at_end_is_one_hunk() {
        let original = b"hello world".to_vec();
        let mut table = PieceTable::from_original(original.clone());
        table.delete(5, 6);
        let patch = build_patch(11, reader(&original), &table).unwrap();
        assert_eq!(patch.hunks.len(), 1);
        assert_eq!(patch.hunks[0].original_offset, 5);
        assert_eq!(patch.hunks[0].original_length, 6);
        assert!(patch.hunks[0].replacement.is_empty());
        round_trip(&original, &table);
    }

    #[test]
    fn replace_in_middle_and_mixed_edits_round_trip() {
        let original = b"the quick brown fox jumps".to_vec();
        let mut table = PieceTable::from_original(original.clone());
        // Replace "quick" (bytes 4..9) with "slow", insert " very"
        // before the space at 14 of the result, delete the tail
        // " jumps".
        table.delete(4, 5);
        table.insert(4, b"slow");
        assert_eq!(materialise(&table), b"the slow brown fox jumps");
        table.insert(14, b" very");
        assert_eq!(materialise(&table), b"the slow brown very fox jumps");
        let len = table.len();
        table.delete(len - 6, 6);
        assert_eq!(materialise(&table), b"the slow brown very fox");
        let patch = build_patch(original.len() as u64, reader(&original), &table).unwrap();
        // Three separate change regions.
        assert_eq!(patch.hunks.len(), 3);
        round_trip(&original, &table);
    }

    #[test]
    fn zero_run_embeds_as_literal_zeros() {
        let original = b"ab".to_vec();
        let mut table = PieceTable::from_original(original.clone());
        table.insert_zeroes(1, 3);
        let patch = build_patch(2, reader(&original), &table).unwrap();
        assert_eq!(patch.hunks.len(), 1);
        assert_eq!(patch.hunks[0].replacement, vec![0u8; 3]);
        round_trip(&original, &table);
    }

    #[test]
    fn duplicated_original_bytes_are_embedded() {
        // The document repeats the original's first half at the
        // end: the repeated bytes cannot be referenced by offset
        // (the walk has passed them), so they embed. The editor's
        // own operations never reorder pieces, so this table is
        // assembled by hand from the public fields; the patch walk
        // and the slice reader iterate pieces linearly and never
        // consult the Fenwick index, which is the only state left
        // stale by the hand assembly.
        let original = b"abcd".to_vec();
        let mut dup = PieceTable::from_original(original.clone());
        dup.pieces = vec![
            crate::piece_table::PieceNode {
                source: BufferSource::Original,
                source_offset: 0,
                length: 4,
            },
            crate::piece_table::PieceNode {
                source: BufferSource::Original,
                source_offset: 0,
                length: 2,
            },
        ];
        dup.total_length = 6;
        let patch = build_patch(4, reader(&original), &dup).unwrap();
        assert_eq!(materialise(&dup), b"abcdab");
        assert_eq!(patch.hunks.len(), 1);
        assert_eq!(patch.hunks[0].replacement, b"ab");
        round_trip(&original, &dup);
    }

    #[test]
    fn parse_rejects_corruption() {
        let original = b"hello world".to_vec();
        let mut table = PieceTable::from_original(original.clone());
        table.delete(5, 6);
        let patch = build_patch(11, reader(&original), &table).unwrap();
        let bytes = patch.to_bytes();

        let mut bad = bytes.clone();
        bad[0] = b'X';
        assert!(matches!(Patch::from_bytes(&bad), Err(PatchError::BadMagic)));
        assert!(matches!(
            Patch::from_bytes(&bytes[..bytes.len() - 1]),
            Err(PatchError::Truncated)
        ));
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(matches!(
            Patch::from_bytes(&extra),
            Err(PatchError::TrailingBytes)
        ));
        // Corrupt the declared result length (bytes 16..24).
        let mut bad = bytes.clone();
        bad[16] ^= 0xFF;
        assert!(matches!(
            Patch::from_bytes(&bad),
            Err(PatchError::ResultLengthMismatch)
        ));
        // Applying against the wrong original length is refused.
        let mut out: Vec<u8> = Vec::new();
        assert!(matches!(
            apply_patch(10, reader(&original), &patch, &mut out),
            Err(PatchError::OriginalLengthMismatch { .. })
        ));
    }
}
