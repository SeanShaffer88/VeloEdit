//! Delta-log crash recovery (roadmap item 7): record *what
//! changed*, not a copy of the document.
//!
//! The old recovery path snapshots document content, so it has to
//! stop at a size cap. This module instead serializes the document
//! *state* — which pieces of the original file are still visible,
//! and the exact bytes of every added piece — alongside a
//! fingerprint of the original file (its path, size, modification
//! stamp, and a streamed SHA-256, see [`crate::hash`]). Rebuilding
//! re-opens the original, verifies the fingerprint, and reassembles
//! a [`PieceTable`] that reads byte-identically to the document
//! that was captured. When the original no longer matches its
//! fingerprint the rebuild refuses with a clear error instead of
//! quietly producing a wrong document.
//!
//! Format (all integers little-endian):
//!
//! ```text
//! magic          8 bytes   "VELOREC1"
//! encoding       u8        0 = UTF-8, 1 = UTF-16 LE, 2 = UTF-16 BE, 3 = ANSI
//! caret          u64       caret byte offset at capture time
//! original size  u64
//! original mtime i64       nanoseconds since the Unix epoch
//! original hash  32 bytes  SHA-256 of the original file
//! path           u32 length + UTF-8 bytes
//! piece count    u32
//! pieces         per piece: u8 tag
//!                  0 (Original): u64 start, u64 length
//!                  1 (Added):    u64 length + the bytes themselves
//! ```
//!
//! Paths are stored as UTF-8; a path that is not valid UTF-8 is
//! recorded lossily (the same trade the settings file makes). This
//! module stands alone for now — the GUI keeps using its snapshot
//! recovery until the wiring slice adopts this format.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::bookmarks::{stamp_of, FileStamp};
use crate::encoding::TextEncoding;
use crate::hash::{Sha256, StreamingHash};
use crate::piece_table::{BufferSource, PieceNode, PieceTable};

const MAGIC: &[u8; 8] = b"VELOREC1";
const TAG_ORIGINAL: u8 = 0;
const TAG_ADDED: u8 = 1;

/// One piece of a captured document: either a run of the original
/// file, or bytes added by editing (stored inline).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryPiece {
    Original { start: u64, len: u64 },
    Added { bytes: Vec<u8> },
}

/// A captured document state (see the module docs for the format).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryState {
    /// Path of the original file, as recorded (UTF-8, lossy when
    /// the real path is not valid UTF-8).
    pub original_path: String,
    /// Size of the original file at capture time.
    pub original_size: u64,
    /// Modification stamp of the original file at capture time
    /// (nanoseconds since the Unix epoch).
    pub original_mtime_nanos: i64,
    /// SHA-256 of the original file's bytes at capture time.
    pub original_sha256: [u8; 32],
    /// The visible document as pieces over the original and the
    /// added bytes, in order.
    pub pieces: Vec<RecoveryPiece>,
    /// Caret byte offset at capture time.
    pub caret: u64,
    /// The document's text encoding.
    pub encoding: TextEncoding,
}

/// The original file's fingerprint: size, modification stamp, and
/// streamed SHA-256 — everything [`RecoveryState::rebuild`] checks
/// before trusting the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileFingerprint {
    pub size: u64,
    pub mtime_nanos: i64,
    pub sha256: [u8; 32],
}

/// Why a recovery operation failed.
#[derive(Debug)]
pub enum RecoveryError {
    /// Reading or writing a file failed.
    Io(io::Error),
    /// The serialized state is malformed.
    Corrupt(String),
    /// The original file no longer matches the fingerprint the
    /// state was captured against (resized, re-stamped, or its
    /// bytes changed). The message says which check failed.
    OriginalChanged(String),
}

impl std::fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecoveryError::Io(err) => write!(f, "recovery file error: {err}"),
            RecoveryError::Corrupt(why) => write!(f, "recovery data is corrupt: {why}"),
            RecoveryError::OriginalChanged(why) => {
                write!(
                    f,
                    "the original file changed since the edits were recorded: {why}"
                )
            }
        }
    }
}

impl std::error::Error for RecoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RecoveryError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for RecoveryError {
    fn from(err: io::Error) -> Self {
        RecoveryError::Io(err)
    }
}

/// Fingerprint the file at `path`: its metadata stamp plus its
/// SHA-256, computed by streaming 64 KiB windows so the file is
/// never held in memory whole.
pub fn fingerprint_file(path: &Path) -> Result<FileFingerprint, RecoveryError> {
    let stamp: FileStamp = stamp_of(&std::fs::metadata(path)?);
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut window = vec![0u8; crate::hash::HASH_WINDOW];
    loop {
        let read = file.read(&mut window)?;
        if read == 0 {
            break;
        }
        hasher.update(&window[..read]);
    }
    Ok(FileFingerprint {
        size: stamp.size,
        mtime_nanos: stamp.mtime_nanos,
        sha256: hasher.finalize(),
    })
}

/// Capture `table`'s current document state against the original
/// file's fingerprint. `original_path` / `fingerprint` describe the
/// file the table's `Original` pieces read from — take the
/// fingerprint with [`fingerprint_file`] when the document is
/// opened or saved, so capture itself never re-reads the file.
pub fn capture(
    table: &PieceTable,
    original_path: &Path,
    fingerprint: &FileFingerprint,
    caret: u64,
    encoding: TextEncoding,
) -> RecoveryState {
    let mut pieces = Vec::with_capacity(table.pieces.len());
    for piece in &table.pieces {
        match piece.source {
            BufferSource::Original => pieces.push(RecoveryPiece::Original {
                start: piece.source_offset,
                len: piece.length,
            }),
            BufferSource::Add => {
                let start = piece.source_offset as usize;
                let end = start + piece.length as usize;
                pieces.push(RecoveryPiece::Added {
                    bytes: table.append_buffer[start..end].to_vec(),
                });
            }
            // A zero run has no stored bytes; the record stores
            // them as an Added piece holding the zeros themselves,
            // so a rebuild reproduces the document byte for byte
            // with no format change. (A very large run therefore
            // inflates its recovery record — the run's bytes are
            // the record's payload by design here.)
            BufferSource::Zeroes => {
                pieces.push(RecoveryPiece::Added {
                    bytes: vec![0u8; piece.length as usize],
                });
            }
        }
    }
    RecoveryState {
        original_path: original_path.to_string_lossy().into_owned(),
        original_size: fingerprint.size,
        original_mtime_nanos: fingerprint.mtime_nanos,
        original_sha256: fingerprint.sha256,
        pieces,
        caret,
        encoding,
    }
}

fn encoding_tag(enc: TextEncoding) -> u8 {
    match enc {
        TextEncoding::Utf8 => 0,
        TextEncoding::Utf16Le => 1,
        TextEncoding::Utf16Be => 2,
        TextEncoding::Ansi => 3,
    }
}

fn encoding_from_tag(tag: u8) -> Result<TextEncoding, RecoveryError> {
    match tag {
        0 => Ok(TextEncoding::Utf8),
        1 => Ok(TextEncoding::Utf16Le),
        2 => Ok(TextEncoding::Utf16Be),
        3 => Ok(TextEncoding::Ansi),
        other => Err(RecoveryError::Corrupt(format!(
            "unknown encoding tag {other}"
        ))),
    }
}

/// Serialize a captured state (format in the module docs).
pub fn serialize(state: &RecoveryState) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.push(encoding_tag(state.encoding));
    out.extend_from_slice(&state.caret.to_le_bytes());
    out.extend_from_slice(&state.original_size.to_le_bytes());
    out.extend_from_slice(&state.original_mtime_nanos.to_le_bytes());
    out.extend_from_slice(&state.original_sha256);
    let path = state.original_path.as_bytes();
    out.extend_from_slice(&(path.len() as u32).to_le_bytes());
    out.extend_from_slice(path);
    out.extend_from_slice(&(state.pieces.len() as u32).to_le_bytes());
    for piece in &state.pieces {
        match piece {
            RecoveryPiece::Original { start, len } => {
                out.push(TAG_ORIGINAL);
                out.extend_from_slice(&start.to_le_bytes());
                out.extend_from_slice(&len.to_le_bytes());
            }
            RecoveryPiece::Added { bytes } => {
                out.push(TAG_ADDED);
                out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
                out.extend_from_slice(bytes);
            }
        }
    }
    out
}

/// A bounds-checked little-endian reader over the serialized form.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], RecoveryError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| RecoveryError::Corrupt("length overflow".to_string()))?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or_else(|| RecoveryError::Corrupt("unexpected end of data".to_string()))?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, RecoveryError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, RecoveryError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64, RecoveryError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }

    fn i64(&mut self) -> Result<i64, RecoveryError> {
        Ok(i64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }
}

/// Parse a serialized state. Malformed input is a
/// [`RecoveryError::Corrupt`] error, never a panic.
pub fn deserialize(bytes: &[u8]) -> Result<RecoveryState, RecoveryError> {
    let mut cursor = Cursor { bytes, pos: 0 };
    if cursor.take(8)? != MAGIC {
        return Err(RecoveryError::Corrupt(
            "bad magic — not a VeloEdit recovery file".to_string(),
        ));
    }
    let encoding = encoding_from_tag(cursor.u8()?)?;
    let caret = cursor.u64()?;
    let original_size = cursor.u64()?;
    let original_mtime_nanos = cursor.i64()?;
    let mut sha256 = [0u8; 32];
    sha256.copy_from_slice(cursor.take(32)?);
    let path_len = cursor.u32()? as usize;
    let path_bytes = cursor.take(path_len)?;
    let original_path = String::from_utf8(path_bytes.to_vec())
        .map_err(|_| RecoveryError::Corrupt("the recorded path is not valid UTF-8".to_string()))?;
    let piece_count = cursor.u32()? as usize;
    // Each piece costs at least 9 serialized bytes, so a count
    // larger than the remaining input is certainly corrupt; the
    // check also keeps a hostile count from forcing a huge
    // allocation up front.
    let mut pieces = Vec::with_capacity(piece_count.min(bytes.len() / 9 + 1));
    for _ in 0..piece_count {
        match cursor.u8()? {
            TAG_ORIGINAL => {
                let start = cursor.u64()?;
                let len = cursor.u64()?;
                pieces.push(RecoveryPiece::Original { start, len });
            }
            TAG_ADDED => {
                let len = cursor.u64()? as usize;
                let data = cursor.take(len)?;
                pieces.push(RecoveryPiece::Added {
                    bytes: data.to_vec(),
                });
            }
            other => return Err(RecoveryError::Corrupt(format!("unknown piece tag {other}"))),
        }
    }
    Ok(RecoveryState {
        original_path,
        original_size,
        original_mtime_nanos,
        original_sha256: sha256,
        pieces,
        caret,
        encoding,
    })
}

impl RecoveryState {
    /// Serialize this state (see [`serialize`]).
    pub fn to_bytes(&self) -> Vec<u8> {
        serialize(self)
    }

    /// Parse a state (see [`deserialize`]).
    pub fn from_bytes(bytes: &[u8]) -> Result<RecoveryState, RecoveryError> {
        deserialize(bytes)
    }

    /// The captured document's total length in bytes.
    pub fn document_len(&self) -> u64 {
        self.pieces
            .iter()
            .map(|piece| match piece {
                RecoveryPiece::Original { len, .. } => *len,
                RecoveryPiece::Added { bytes } => bytes.len() as u64,
            })
            .sum()
    }

    /// Rebuild the captured document as a [`PieceTable`].
    ///
    /// The original file is re-opened and fingerprinted first:
    /// size, modification stamp, and SHA-256 must all match the
    /// capture, or the rebuild refuses with
    /// [`RecoveryError::OriginalChanged`] — a recovery that quietly
    /// spliced edits into a different file would be worse than no
    /// recovery at all. On success the rebuilt table reads
    /// byte-identically to the captured document.
    pub fn rebuild(&self) -> Result<PieceTable, RecoveryError> {
        let path = PathBuf::from(&self.original_path);
        let now = fingerprint_file(&path)?;
        if now.size != self.original_size {
            return Err(RecoveryError::OriginalChanged(format!(
                "its size is {} bytes now, {} when the edits were recorded",
                now.size, self.original_size
            )));
        }
        if now.mtime_nanos != self.original_mtime_nanos {
            return Err(RecoveryError::OriginalChanged(
                "its modification time differs from when the edits were recorded".to_string(),
            ));
        }
        if now.sha256 != self.original_sha256 {
            return Err(RecoveryError::OriginalChanged(
                "its contents hash differently from when the edits were recorded".to_string(),
            ));
        }
        // Range-check the Original pieces against the verified
        // size before trusting them.
        for piece in &self.pieces {
            if let RecoveryPiece::Original { start, len } = piece {
                if start
                    .checked_add(*len)
                    .map_or(true, |end| end > self.original_size)
                {
                    return Err(RecoveryError::Corrupt(format!(
                        "an original piece [{start}, +{len}) runs past the file's {} bytes",
                        self.original_size
                    )));
                }
            }
        }
        let bytes = std::fs::read(&path)?;
        let mut table = PieceTable::from_original(bytes);
        // Rebuild the exact piece layout: Added bytes are
        // concatenated into the append buffer in piece order, and
        // the piece list is rewritten to the captured layout (the
        // same direct-rewrite pattern the undo engine uses, closed
        // by a weights rebuild).
        let mut append: Vec<u8> = Vec::new();
        let mut pieces: Vec<PieceNode> = Vec::with_capacity(self.pieces.len());
        let mut total: u64 = 0;
        for piece in &self.pieces {
            match piece {
                RecoveryPiece::Original { start, len } => {
                    if *len > 0 {
                        pieces.push(PieceNode::new(BufferSource::Original, *start, *len));
                        total += len;
                    }
                }
                RecoveryPiece::Added { bytes } => {
                    if !bytes.is_empty() {
                        pieces.push(PieceNode::new(
                            BufferSource::Add,
                            append.len() as u64,
                            bytes.len() as u64,
                        ));
                        append.extend_from_slice(bytes);
                        total += bytes.len() as u64;
                    }
                }
            }
        }
        table.append_buffer = append;
        table.pieces = pieces;
        table.total_length = total;
        table.rebuild_weights();
        Ok(table)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::hash_bytes;

    /// A unique temp directory per test, removed by the caller.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "veloedit-recovery-test-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build an edited document over a real file on disk, capture
    /// its state, and return everything the round trip needs.
    fn edited_fixture(dir: &Path) -> (PathBuf, RecoveryState, Vec<u8>) {
        let path = dir.join("doc.txt");
        let original = b"hello world, this is the original text.".to_vec();
        std::fs::write(&path, &original).unwrap();
        let fingerprint = fingerprint_file(&path).unwrap();
        let mut table = PieceTable::from_original(original);
        table.insert(5, b" brave new");
        table.delete(20, 4);
        table.insert(table.len(), b"!");
        let expected = table.read(0, table.len());
        let state = capture(&table, &path, &fingerprint, 7, TextEncoding::Utf16Le);
        (path, state, expected)
    }

    #[test]
    fn serialize_round_trip_preserves_the_state() {
        let dir = temp_dir("serde");
        let (_path, state, _expected) = edited_fixture(&dir);
        let back = RecoveryState::from_bytes(&state.to_bytes()).unwrap();
        assert_eq!(back, state);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rebuild_recreates_the_edited_document_byte_identical() {
        let dir = temp_dir("rebuild");
        let (_path, state, expected) = edited_fixture(&dir);
        // Go through the serialized form, as a real restart would.
        let state = RecoveryState::from_bytes(&state.to_bytes()).unwrap();
        let mut rebuilt = state.rebuild().unwrap();
        assert_eq!(rebuilt.len(), expected.len() as u64);
        assert_eq!(rebuilt.read(0, rebuilt.len()), expected);
        assert!(rebuilt.check_invariants());
        // The rebuilt table keeps editing normally.
        rebuilt.insert(0, b">> ");
        let mut want = b">> ".to_vec();
        want.extend_from_slice(&expected);
        assert_eq!(rebuilt.read(0, rebuilt.len()), want);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rebuild_refuses_a_tampered_original() {
        let dir = temp_dir("tamper");
        let (path, state, _expected) = edited_fixture(&dir);
        // Same length, different bytes: size and stamp move too on
        // most filesystems, so also pin the hash path by restoring
        // the recorded stamp afterwards is not possible portably —
        // any one mismatch is enough to refuse.
        std::fs::write(&path, b"HELLO WORLD, THIS IS THE ORIGINAL TEXT.").unwrap();
        let err = state.rebuild().unwrap_err();
        assert!(
            matches!(err, RecoveryError::OriginalChanged(_)),
            "unexpected error: {err}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rebuild_refuses_a_resized_original() {
        let dir = temp_dir("resize");
        let (path, state, _expected) = edited_fixture(&dir);
        std::fs::write(&path, b"shorter").unwrap();
        let err = state.rebuild().unwrap_err();
        assert!(matches!(err, RecoveryError::OriginalChanged(_)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fingerprint_matches_the_standalone_hasher() {
        let dir = temp_dir("fingerprint");
        let path = dir.join("data.bin");
        let data: Vec<u8> = (0u8..=255).cycle().take(200_000).collect();
        std::fs::write(&path, &data).unwrap();
        let fingerprint = fingerprint_file(&path).unwrap();
        assert_eq!(fingerprint.size, 200_000);
        assert_eq!(
            crate::hash::to_hex(&fingerprint.sha256),
            hash_bytes(&data).sha256
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupt_inputs_are_errors_not_panics() {
        assert!(RecoveryState::from_bytes(&[]).is_err());
        assert!(RecoveryState::from_bytes(b"VELOREC1").is_err());
        assert!(RecoveryState::from_bytes(b"NOTRECOV........").is_err());
        // A valid prefix truncated mid-piece.
        let dir = temp_dir("truncated");
        let (_path, state, _expected) = edited_fixture(&dir);
        let bytes = state.to_bytes();
        assert!(RecoveryState::from_bytes(&bytes[..bytes.len() - 3]).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unedited_document_recovers_as_the_original() {
        let dir = temp_dir("unedited");
        let path = dir.join("plain.txt");
        std::fs::write(&path, b"nothing changed").unwrap();
        let fingerprint = fingerprint_file(&path).unwrap();
        let table = PieceTable::from_original(b"nothing changed".to_vec());
        let state = capture(&table, &path, &fingerprint, 0, TextEncoding::Utf8);
        assert_eq!(state.document_len(), 15);
        let rebuilt = state.rebuild().unwrap();
        assert_eq!(rebuilt.read(0, rebuilt.len()), b"nothing changed");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
