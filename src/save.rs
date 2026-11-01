//! Atomic save pipeline (SPEC.md section 5.1) — a safety path.
//!
//! Saving never writes to the target file in place. The document is
//! streamed to a staging file in the target's own directory
//! (`<filename>.tmp_velo_<pid>`), the staging file is flushed to disk,
//! and only then is it swapped over the target in one atomic step
//! (`ReplaceFileW` on Windows; `rename` elsewhere, which is an atomic
//! replace on POSIX). A crash or failure at any point before the swap
//! leaves the original file byte-identical; the staging file is removed
//! on every failure path.
//!
//! Streaming: the writer consumes [`PieceTable::iter_slices`] and writes
//! in chunks of at most [`SaveOptions::max_chunk`] bytes (4 MB by
//! default, per the spec), so the document is never materialised in
//! memory, however large it is.
//!
//! Save-time transforms (roadmap items 31/32): [`SaveOptions`] can
//! also ask for the line endings to be normalised
//! ([`EolChoice::Lf`] / [`EolChoice::Crlf`]) and for the byte-order
//! mark to be added or removed ([`BomChoice`]), applied as a
//! streaming transform over the same chunks — correct across chunk
//! and piece boundaries, in the document encoding's unit width.
//! The default `Keep` / `Keep` is a byte-identical pass-through:
//! the staging writer takes its original code path, untouched.
//!
//! [`PieceTable::save_to`] adds the spec's step 6 on top of
//! [`save_atomic`]: after a successful swap it closes *our own* mapping
//! of the target (closed before the swap — step 4 — via
//! [`PieceTable::detach_original`]), re-opens the saved file as the new
//! original backing (re-mapped on Windows, read back into memory on the
//! host), clears the append buffer, and resets the piece list to a
//! single `Original` piece. **Save is a history boundary:** the
//! [`UndoStack`] passed in is cleared, because the history entries point
//! at append-buffer bytes and piece segments that the reset invalidates.
//! Nothing recorded before a successful save can be undone after it.
//! (Deviation note: the stack is a separate value from the table in this
//! codebase — every operation takes both — so `save_to` takes it as a
//! parameter in order to clear it in the same operation, rather than
//! letting a caller forget.)

// This module is one of the two allowed-unsafe sites (PROJECT_RULES.md,
// unsafe confinement rule): the Windows half calls `ReplaceFileW` and
// `FlushFileBuffers`. Every unsafe block carries a // SAFETY: comment;
// the non-Windows code in this file contains no unsafe at all.
#![allow(unsafe_code)]

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::encoding::TextEncoding;
use crate::piece_table::{OriginalBuffer, PieceTable};
use crate::undo::UndoStack;

/// Default streaming chunk size: the 4 MB blocks of SPEC.md section 5.1.
pub const DEFAULT_MAX_CHUNK: usize = 4 * 1024 * 1024;

/// Line-ending handling for a save (roadmap item 31).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EolChoice {
    /// Write line endings exactly as they are in the document.
    Keep,
    /// Normalise every line ending (CRLF, lone CR, lone LF) to LF.
    Lf,
    /// Normalise every line ending to CRLF.
    Crlf,
}

/// Byte-order-mark handling for a save (roadmap item 32).
/// The mark in question is the one for the save's encoding
/// ([`crate::encoding::bom_bytes`]); ANSI has none, so both actions
/// are no-ops there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BomChoice {
    /// Leave the start of the document exactly as it is.
    Keep,
    /// Ensure the document starts with the encoding's BOM
    /// (added when missing, never doubled).
    Add,
    /// Remove the encoding's BOM when the document starts with it.
    Remove,
}

/// Options for the save operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SaveOptions {
    /// Largest single write issued while streaming the staging file.
    /// Must be greater than zero.
    pub max_chunk: usize,
    /// The document's encoding: the unit width the line-ending
    /// transform works in (1 byte for UTF-8/ANSI, 2 for UTF-16)
    /// and the encoding whose BOM [`BomChoice`] refers to.
    /// Irrelevant when both transforms are `Keep`.
    pub encoding: TextEncoding,
    /// Line-ending transform (roadmap item 31).
    pub eol: EolChoice,
    /// Byte-order-mark transform (roadmap item 32).
    pub bom: BomChoice,
    /// Block-clone fast save (slice R4c, roadmap item 4): when
    /// true, an in-place save of a mapped document first tries
    /// building the staging file by cloning the original file's
    /// extents and writing only the changed hunks
    /// (`write_staging_clone`); any failure falls back to the
    /// normal streamed write with no error shown. Only honoured
    /// by [`PieceTable::save_to_with_options`] (the in-place
    /// path) with both transforms `Keep`; `save_atomic` always
    /// streams. Ignored elsewhere.
    pub fast_clone: bool,
}

impl Default for SaveOptions {
    fn default() -> Self {
        Self {
            max_chunk: DEFAULT_MAX_CHUNK,
            encoding: TextEncoding::Utf8,
            eol: EolChoice::Keep,
            bom: BomChoice::Keep,
            fast_clone: false,
        }
    }
}

/// Which step of the save pipeline failed. Named in every error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveStep {
    /// Creating or opening the staging file.
    Staging,
    /// Streaming the document into the staging file.
    Write,
    /// Flushing the staging file to disk.
    Flush,
    /// The atomic swap over the target.
    Swap,
    /// Re-opening the saved file as the new original backing.
    Remap,
}

impl std::fmt::Display for SaveStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            SaveStep::Staging => "staging",
            SaveStep::Write => "write",
            SaveStep::Flush => "flush",
            SaveStep::Swap => "swap",
            SaveStep::Remap => "remap",
        };
        f.write_str(name)
    }
}

/// Error from the save pipeline: the failed step plus the OS error.
#[derive(Debug)]
pub struct SaveError {
    pub step: SaveStep,
    pub source: io::Error,
}

impl SaveError {
    fn new(step: SaveStep, source: io::Error) -> Self {
        Self { step, source }
    }
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "save failed at the {} step: {}", self.step, self.source)
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl From<SaveError> for io::Error {
    fn from(err: SaveError) -> Self {
        io::Error::new(err.source.kind(), err.to_string())
    }
}

/// Outcome of a successful save.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SaveReport {
    /// Total document bytes written to the staging file (= the saved
    /// document's length).
    pub bytes_written: u64,
    /// True when the staging path no longer exists after the save (the
    /// normal case: the swap moved it over the target).
    pub staging_removed: bool,
    /// True when the staging file was built by the block-clone
    /// fast path (see [`SaveOptions::fast_clone`]). False for every
    /// normal streamed save, including a fast-save request that
    /// fell back.
    pub fast_clone_used: bool,
}

/// The Windows `FILE_ATTRIBUTE_READONLY` bit value.
pub const FILE_ATTRIBUTE_READONLY_BIT: u32 = 0x0000_0001;

/// True when a Windows file-attributes word marks the file
/// read-only (slice R4c, roadmap item 33, "protected files"). The
/// pure half of the shell's protected-file detection, split out so
/// the host suite can pin it down.
pub fn attr_is_read_only(file_attributes: u32) -> bool {
    file_attributes & FILE_ATTRIBUTE_READONLY_BIT != 0
}

/// True when the file at `path` is marked read-only. Uses the
/// cross-platform permissions flag, cross-checked on Windows
/// against the raw attribute bit (the two can disagree for
/// directory-like entries; either one marking the file read-only
/// is enough to treat it as protected). A missing or unreadable
/// file is not "read-only" — it is an ordinary open error, which
/// the caller reports separately.
pub fn path_is_read_only(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if metadata.permissions().readonly() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        attr_is_read_only(metadata.file_attributes())
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Free disk space a save wants to see before it starts streaming:
/// the document's length plus a twentieth (1.05× in total, roadmap
/// item 5). Saturates instead of overflowing for absurd lengths.
pub fn space_needed(file_len: u64) -> u64 {
    file_len.saturating_add(file_len / 20)
}

/// Check that the target's directory has at least [`space_needed`]
/// bytes free, before a single byte is written (roadmap item 5).
/// Failing halfway through a multi-gigabyte streaming write — after
/// minutes of work, with the staging file half-written — is the
/// failure this exists to prevent.
///
/// Windows queries `GetDiskFreeSpaceExW`. If the query itself fails
/// the check is skipped (fail-open): the save then behaves exactly as
/// it did before this check existed, rather than being blocked by an
/// API hiccup. Non-Windows builds skip the check entirely.
fn check_free_space(target: &Path, doc_len: u64) -> Result<(), SaveError> {
    #[cfg(windows)]
    {
        check_free_space_windows(target, doc_len)
    }
    #[cfg(not(windows))]
    {
        let _ = (target, doc_len);
        Ok(())
    }
}

/// The Windows half of [`check_free_space`].
#[cfg(windows)]
fn check_free_space_windows(target: &Path, doc_len: u64) -> Result<(), SaveError> {
    use std::os::windows::ffi::OsStrExt;

    let dir = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free_available: u64 = 0;
    // SAFETY: the wide string is NUL-terminated and outlives the call;
    // the three out-slots are writable u64s. Only the caller-available
    // figure is read afterwards, and only when the call succeeded.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        // Query failed: skip the check (see check_free_space).
        return Ok(());
    }
    let needed = space_needed(doc_len);
    if free_available < needed {
        return Err(SaveError::new(
            SaveStep::Staging,
            io::Error::other(format!(
                "not enough free disk space to save safely: the document needs about \
                 {needed} bytes free next to the target, only {free_available} bytes are free"
            )),
        ));
    }
    Ok(())
}

/// Clamp a `[start, start + length)` range to a document of `total`
/// bytes (roadmap items 35 and 44). Returns `None` when `start` is
/// beyond the end of the document; a start exactly at the end yields
/// an empty range; the length is shortened to what actually exists.
pub fn clamp_range(start: u64, length: u64, total: u64) -> Option<(u64, u64)> {
    if start > total {
        return None;
    }
    Some((start, length.min(total - start)))
}

/// Stream `length` bytes of `table` starting at `start` to `out`, in
/// chunks of at most [`DEFAULT_MAX_CHUNK`] bytes, and return the
/// number of bytes written. The range is clamped to the document
/// (see [`clamp_range`]); a start beyond the end of the document is
/// an error and nothing is written.
pub fn write_table_range(
    table: &PieceTable,
    start: u64,
    length: u64,
    out: &mut impl Write,
) -> io::Result<u64> {
    let Some((start, length)) = clamp_range(start, length, table.len()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "range start {start} is beyond the end of the document ({} bytes)",
                table.len()
            ),
        ));
    };
    let mut pos = start;
    let mut remaining = length;
    while remaining > 0 {
        let step = remaining.min(DEFAULT_MAX_CHUNK as u64);
        let chunk = table.read(pos, step);
        if chunk.is_empty() {
            break; // defensive: a clamped range always has its bytes
        }
        out.write_all(&chunk)?;
        pos += chunk.len() as u64;
        remaining -= chunk.len() as u64;
    }
    Ok(length - remaining)
}

/// Stream a byte range of `table` (see [`write_table_range`]) into a
/// new file at `dest`, replacing any file already there. The range is
/// validated before the file is created, so an out-of-range request
/// never truncates an existing file.
pub fn write_range_to_file(
    table: &PieceTable,
    start: u64,
    length: u64,
    dest: &Path,
) -> io::Result<u64> {
    if clamp_range(start, length, table.len()).is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "range start {start} is beyond the end of the document ({} bytes)",
                table.len()
            ),
        ));
    }
    let mut file = File::create(dest)?;
    let written = write_table_range(table, start, length, &mut file)?;
    file.flush()?;
    Ok(written)
}

/// Largest number of parts [`split_file`] will produce: a split is a
/// convenience for moving a file around, not a sharding scheme, so an
/// absurd part count is a mistake to refuse, not a job to start.
pub const MAX_SPLIT_PARTS: u64 = 10_000;

/// How many parts a file of `file_len` bytes makes at `part_size`
/// bytes each (roadmap item 36). An empty file still makes one (empty)
/// part, so a split always produces something copyable.
fn split_part_count(file_len: u64, part_size: u64) -> u64 {
    debug_assert!(part_size > 0);
    if file_len == 0 {
        1
    } else {
        file_len.div_ceil(part_size)
    }
}

/// The file name of part `index` (1-based) of a split of `file_name`
/// (roadmap item 36): `<name>.part001`, `.part002`, … — at least three
/// digits, growing (`.part10000`) when the part count needs more, so
/// the names always sort in part order.
fn split_part_name(file_name: &str, index: u64, count: u64) -> String {
    let width = ((count.max(1).to_string().len()).max(3)).max(index.to_string().len());
    format!("{file_name}.part{index:0width$}")
}

/// Split the file at `src` into parts of at most `part_size` bytes in
/// `out_dir` (created when missing), streamed in chunks of at most
/// [`DEFAULT_MAX_CHUNK`] bytes — the source is never loaded whole
/// (roadmap item 36). Parts are named `<name>.part001`, `.part002`, …
/// (see [`split_part_name`]) and returned in order; joining them back
/// in that order reproduces the source byte for byte.
///
/// Refusals, all before anything is written: `part_size` of 0, and a
/// part count above [`MAX_SPLIT_PARTS`]. An empty source produces a
/// single empty part.
pub fn split_file(src: &Path, part_size: u64, out_dir: &Path) -> io::Result<Vec<PathBuf>> {
    if part_size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the part size must be greater than zero",
        ));
    }
    let file_len = std::fs::metadata(src)?.len();
    let count = split_part_count(file_len, part_size);
    if count > MAX_SPLIT_PARTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "splitting {} bytes into {part_size}-byte parts would make {count} parts; \
                 the limit is {MAX_SPLIT_PARTS} — use a larger part size",
                file_len
            ),
        ));
    }
    let file_name = src
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "veloedit".to_string());
    std::fs::create_dir_all(out_dir)?;

    let mut input = File::open(src)?;
    let mut paths: Vec<PathBuf> = Vec::with_capacity(count as usize);
    let mut buffer = vec![0u8; DEFAULT_MAX_CHUNK];
    for index in 1..=count {
        let path = out_dir.join(split_part_name(&file_name, index, count));
        let mut part = File::create(&path)?;
        // This part's share: a full part_size, except the last part,
        // which takes whatever is left (0 for an empty source).
        let mut remaining = part_size.min(file_len - (index - 1) * part_size);
        while remaining > 0 {
            let want = remaining.min(buffer.len() as u64) as usize;
            let read = std::io::Read::read(&mut input, &mut buffer[..want])?;
            if read == 0 {
                break; // the source shrank mid-split; the parts hold what exists
            }
            part.write_all(&buffer[..read])?;
            remaining -= read as u64;
        }
        part.flush()?;
        paths.push(path);
    }
    Ok(paths)
}

/// Build the staging path for `target` (SPEC.md section 5.1 step 1):
/// same directory, name `<filename>.tmp_velo_<pid>` where `<filename>`
/// is the target's full file name (extension included).
pub fn staging_path_for(target: &Path) -> PathBuf {
    let file_name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "veloedit".to_string());
    let staging_name = format!("{file_name}.tmp_velo_{}", std::process::id());
    match target.parent() {
        Some(parent) => parent.join(staging_name),
        None => PathBuf::from(staging_name),
    }
}

/// The streaming line-ending / BOM transform (roadmap items 31/32),
/// fed the document in arbitrary chunks and emitting transformed
/// bytes into `out`.
///
/// Two boundary hazards are designed for:
///
/// * A `\r` at the end of a chunk is *held* (as `pending_cr`) until
///   the next unit is known — emitted alone, it would turn a CRLF
///   pair split across chunks into two line endings.
/// * For UTF-16, a unit itself can straddle a chunk boundary; the
///   partial unit waits in `unit_hold` for its other byte.
///
/// The BOM decision similarly holds the first bytes until enough
/// have arrived to tell whether the document already starts with
/// the encoding's mark.
struct SaveTransform {
    /// Unit width of the encoding (1 or 2 bytes).
    width: usize,
    /// The CR and LF unit patterns (`[0x0D]` / `[0x0A]` in 1-byte
    /// encodings, the endian forms in UTF-16).
    cr: [u8; 2],
    lf: [u8; 2],
    /// True when normalising to CRLF, false for LF. Meaningless
    /// when `convert_eol` is false.
    to_crlf: bool,
    convert_eol: bool,
    /// The encoding's BOM bytes (empty for ANSI).
    bom: &'static [u8],
    bom_mode: BomChoice,
    /// False while the first `bom.len()` bytes are still being
    /// collected in `held`.
    bom_decided: bool,
    held: Vec<u8>,
    /// A partial UTF-16 unit (0 or 1 bytes between feeds).
    unit_hold: Vec<u8>,
    /// A CR unit was seen and its ending not yet emitted.
    pending_cr: bool,
}

impl SaveTransform {
    fn new(opts: &SaveOptions) -> Self {
        let (width, cr, lf) = match opts.encoding {
            TextEncoding::Utf8 | TextEncoding::Ansi => (1, [0x0D, 0], [0x0A, 0]),
            TextEncoding::Utf16Le => (2, [0x0D, 0x00], [0x0A, 0x00]),
            TextEncoding::Utf16Be => (2, [0x00, 0x0D], [0x00, 0x0A]),
        };
        let bom = crate::encoding::bom_bytes(opts.encoding);
        Self {
            width,
            cr,
            lf,
            to_crlf: opts.eol == EolChoice::Crlf,
            convert_eol: opts.eol != EolChoice::Keep,
            bom,
            bom_mode: opts.bom,
            // Nothing to decide when keeping the head as-is, or
            // when the encoding has no BOM at all.
            bom_decided: opts.bom == BomChoice::Keep || bom.is_empty(),
            held: Vec::new(),
            unit_hold: Vec::new(),
            pending_cr: false,
        }
    }

    /// Feed one chunk of the document; transformed bytes are
    /// appended to `out` (previous content of `out` is kept).
    fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) {
        if !self.bom_decided {
            self.held.extend_from_slice(input);
            if self.held.len() < self.bom.len() {
                return;
            }
            let held = std::mem::take(&mut self.held);
            self.decide_bom(&held, out);
            return;
        }
        self.feed_body(input, out);
    }

    /// End of input: settle a still-open BOM decision (the whole
    /// document was shorter than the BOM) and flush the held CR /
    /// partial unit.
    fn finish(&mut self, out: &mut Vec<u8>) {
        if !self.bom_decided {
            let held = std::mem::take(&mut self.held);
            // `held` is shorter than the BOM, so the document does
            // not start with it: Remove passes it through as
            // content; Add prepends the mark first.
            if self.bom_mode == BomChoice::Add {
                out.extend_from_slice(self.bom);
            }
            self.bom_decided = true;
            self.feed_body(&held, out);
        }
        if self.pending_cr {
            self.pending_cr = false;
            self.emit_ending(out);
        }
        // A trailing partial unit (only possible for a malformed
        // odd-length UTF-16 document) passes through untouched.
        out.extend_from_slice(&self.unit_hold);
        self.unit_hold.clear();
    }

    /// Apply the BOM choice to the collected head bytes, then run
    /// them through the line-ending body transform.
    fn decide_bom(&mut self, held: &[u8], out: &mut Vec<u8>) {
        self.bom_decided = true;
        match self.bom_mode {
            BomChoice::Keep => self.feed_body(held, out),
            BomChoice::Remove => {
                if held.starts_with(self.bom) {
                    self.feed_body(&held[self.bom.len()..], out);
                } else {
                    self.feed_body(held, out);
                }
            }
            BomChoice::Add => {
                if held.starts_with(self.bom) {
                    self.feed_body(held, out);
                } else {
                    // The mark goes out directly: it contains no
                    // line-ending units, and for UTF-16 it is
                    // exactly one unit wide, so alignment holds.
                    out.extend_from_slice(self.bom);
                    self.feed_body(held, out);
                }
            }
        }
    }

    /// The line-ending body transform over document bytes (the BOM
    /// region has already been dealt with by the caller).
    fn feed_body(&mut self, input: &[u8], out: &mut Vec<u8>) {
        if !self.convert_eol {
            out.extend_from_slice(input);
            return;
        }
        self.unit_hold.extend_from_slice(input);
        let width = self.width;
        let mut processed = 0usize;
        while self.unit_hold.len() - processed >= width {
            let mut unit = [0u8; 2];
            unit[..width].copy_from_slice(&self.unit_hold[processed..processed + width]);
            processed += width;
            self.emit_unit(&unit, out);
        }
        self.unit_hold.drain(..processed);
    }

    /// Emit the target line ending.
    fn emit_ending(&self, out: &mut Vec<u8>) {
        if self.to_crlf {
            out.extend_from_slice(&self.cr[..self.width]);
        }
        out.extend_from_slice(&self.lf[..self.width]);
    }

    /// Process one complete unit, honouring a held CR.
    fn emit_unit(&mut self, unit: &[u8; 2], out: &mut Vec<u8>) {
        let unit = &unit[..self.width];
        if self.pending_cr {
            self.pending_cr = false;
            if unit == &self.lf[..self.width] {
                // CR LF pair: one line ending, both units consumed.
                self.emit_ending(out);
                return;
            }
            // A lone CR: its ending comes out before this unit.
            self.emit_ending(out);
        }
        if unit == &self.cr[..self.width] {
            self.pending_cr = true;
        } else if unit == &self.lf[..self.width] {
            self.emit_ending(out);
        } else {
            out.extend_from_slice(unit);
        }
    }
}

/// Stream the whole document into the staging file at `staging` in
/// chunks of at most `opts.max_chunk` bytes, applying the save
/// transforms `opts` asks for, flush it to disk, and close it.
/// Returns the number of bytes written (the transformed length when
/// a transform is active).
fn write_staging(table: &PieceTable, staging: &Path, opts: &SaveOptions) -> Result<u64, SaveError> {
    debug_assert!(opts.max_chunk > 0);
    let mut file = File::create(staging).map_err(|e| SaveError::new(SaveStep::Staging, e))?;
    let mut written: u64 = 0;
    if opts.eol == EolChoice::Keep && opts.bom == BomChoice::Keep {
        // The original pass-through path, byte for byte: a
        // Keep/Keep save is identical to the pipeline the existing
        // tests pin down.
        for slice in table.iter_slices() {
            for chunk in slice.chunks(opts.max_chunk) {
                file.write_all(chunk)
                    .map_err(|e| SaveError::new(SaveStep::Write, e))?;
                written += chunk.len() as u64;
            }
        }
    } else {
        let mut transform = SaveTransform::new(opts);
        let mut out: Vec<u8> = Vec::new();
        for slice in table.iter_slices() {
            for chunk in slice.chunks(opts.max_chunk) {
                out.clear();
                transform.feed(chunk, &mut out);
                file.write_all(&out)
                    .map_err(|e| SaveError::new(SaveStep::Write, e))?;
                written += out.len() as u64;
            }
        }
        out.clear();
        transform.finish(&mut out);
        if !out.is_empty() {
            file.write_all(&out)
                .map_err(|e| SaveError::new(SaveStep::Write, e))?;
            written += out.len() as u64;
        }
    }
    flush_file(&file).map_err(|e| SaveError::new(SaveStep::Flush, e))?;
    // Close before the swap (the File drops here): ReplaceFileW cannot
    // replace from a file that is still open for writing.
    drop(file);
    Ok(written)
}

/// Block-clone staging build (slice R4c, roadmap item 4): build
/// the staging file by cloning the original file's extents and
/// writing only the changed hunks, instead of streaming the whole
/// document. The hunk list is the delta patch core's
/// ([`crate::patch::build_patch`]): regions between hunks are
/// byte-identical to the original by construction, so they are
/// cloned from it with `FSCTL_DUPLICATE_EXTENTS_TO_FILE` (the
/// filesystem shares the underlying clusters — copy-on-write — so
/// almost nothing is physically written); hunk replacements are
/// written from the patch bytes. The result then finishes through
/// the exact same flush + atomic swap as a normal save.
///
/// Contract: ANY failure returns `None` and removes the staging
/// file, and the caller falls back to the normal streamed
/// [`write_staging`] — no error is ever surfaced from this path.
/// Windows only; the non-Windows form always returns `None`.
#[cfg(not(windows))]
fn write_staging_clone(_table: &PieceTable, _staging: &Path, _source: &Path) -> Option<u64> {
    None
}

/// Windows form of [`write_staging_clone`].
#[cfg(windows)]
fn write_staging_clone(table: &PieceTable, staging: &Path, source: &Path) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Ioctl::{
        DUPLICATE_EXTENTS_DATA, FSCTL_DUPLICATE_EXTENTS_TO_FILE,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    // The delta over the on-disk original: hunks plus the
    // unchanged spans between them (the spans are what gets
    // cloned). The original is read through a plain file here —
    // the table's own mapping stays untouched.
    let mut source_file = File::open(source).ok()?;
    let patch = crate::patch::build_patch(
        table.original_file_size,
        |offset, out| {
            if source_file.seek(SeekFrom::Start(offset)).is_err() {
                return 0;
            }
            source_file.read(out).unwrap_or(0)
        },
        table,
    )
    .ok()?;
    drop(source_file);

    let mut staging_file = File::create(staging).ok()?;
    // Pre-size the staging file to the result length: extent
    // cloning fills ranges of an existing file.
    if staging_file.set_len(patch.result_len).is_err() {
        drop(staging_file);
        let _ = std::fs::remove_file(staging);
        return None;
    }

    // A dedicated read handle on the source: the clone call names
    // the source by handle.
    let wide: Vec<u16> = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives
    // the call; the remaining arguments are plain values / null
    // pointers CreateFileW permits. The handle is checked before
    // any use and closed exactly once below.
    let source_handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if source_handle == INVALID_HANDLE_VALUE {
        drop(staging_file);
        let _ = std::fs::remove_file(staging);
        return None;
    }

    let staging_handle = staging_file.as_raw_handle() as HANDLE;
    // Clone one unchanged span of `len` bytes from the source at
    // `src_off` to the staging file at `dst_off`.
    let clone_span = |src_off: u64, dst_off: u64, len: u64| -> bool {
        let mut remaining = len;
        let mut s = src_off;
        let mut d = dst_off;
        while remaining > 0 {
            let count = remaining.min(i64::MAX as u64);
            let data = DUPLICATE_EXTENTS_DATA {
                FileHandle: source_handle,
                SourceFileOffset: s as i64,
                TargetFileOffset: d as i64,
                ByteCount: count as i64,
            };
            let mut returned: u32 = 0;
            // SAFETY: `staging_handle` is the live staging file's
            // handle; `data` is a valid input struct that outlives
            // the call; no output buffer or overlapped structure
            // is used (the handle is synchronous). The result is
            // checked before anything relies on it.
            let ok = unsafe {
                DeviceIoControl(
                    staging_handle,
                    FSCTL_DUPLICATE_EXTENTS_TO_FILE,
                    &data as *const DUPLICATE_EXTENTS_DATA as *const std::ffi::c_void,
                    std::mem::size_of::<DUPLICATE_EXTENTS_DATA>() as u32,
                    std::ptr::null_mut(),
                    0,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return false;
            }
            s += count;
            d += count;
            remaining -= count;
        }
        true
    };

    // Walk the result in order: clone each unchanged span, write
    // each hunk's replacement bytes at its result position. The
    // patch's hunks are in ascending original-offset order (the
    // patch builder's cursor never moves backwards), so the
    // unchanged span before a hunk is
    // `[orig_pos, hunk.original_offset)`.
    let mut built = true;
    let mut result_pos: u64 = 0;
    let mut orig_pos: u64 = 0;
    for hunk in &patch.hunks {
        let unchanged = hunk.original_offset.saturating_sub(orig_pos);
        if unchanged > 0 && !clone_span(orig_pos, result_pos, unchanged) {
            built = false;
            break;
        }
        result_pos += unchanged;
        if !hunk.replacement.is_empty() {
            if staging_file.seek(SeekFrom::Start(result_pos)).is_err()
                || staging_file.write_all(&hunk.replacement).is_err()
            {
                built = false;
                break;
            }
            result_pos += hunk.replacement.len() as u64;
        }
        orig_pos = hunk.original_offset + hunk.original_length;
    }
    if built {
        let tail = table.original_file_size.saturating_sub(orig_pos);
        if tail > 0 && !clone_span(orig_pos, result_pos, tail) {
            built = false;
        } else {
            result_pos += tail;
        }
    }
    if built && (result_pos != patch.result_len || flush_file(&staging_file).is_err()) {
        built = false;
    }

    // SAFETY: `source_handle` was opened by us above and is closed
    // exactly once, here, on every path past its creation.
    unsafe { CloseHandle(source_handle) };
    drop(staging_file);
    if built {
        Some(patch.result_len)
    } else {
        let _ = std::fs::remove_file(staging);
        None
    }
}

/// Flush an open file's data through the drive cache to disk: on Windows
/// this is `FlushFileBuffers` (SPEC.md section 5.1 step 3); elsewhere
/// `File::sync_all` is the equivalent.
#[cfg(not(windows))]
fn flush_file(file: &File) -> io::Result<()> {
    file.sync_all()
}

/// Flush an open file's data through the drive cache to disk: on Windows
/// this is `FlushFileBuffers` (SPEC.md section 5.1 step 3); elsewhere
/// `File::sync_all` is the equivalent.
#[cfg(windows)]
fn flush_file(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    flush_file_buffers(file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE)
}

/// The `FlushFileBuffers` call itself — the second allowed-unsafe site
/// in this module's Windows half (the other is `ReplaceFileW`).
#[cfg(windows)]
fn flush_file_buffers(handle: windows_sys::Win32::Foundation::HANDLE) -> io::Result<()> {
    // SAFETY: `handle` is the raw handle of a File owned by the caller,
    // which outlives this call; FlushFileBuffers on an open file handle
    // is all the API requires.
    let ok = unsafe { windows_sys::Win32::Storage::FileSystem::FlushFileBuffers(handle) };
    if ok == 0 {
        // SAFETY: GetLastError is a plain thread-local read, called
        // immediately after the failing call.
        let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    Ok(())
}

/// The atomic swap (SPEC.md section 5.1 step 5), non-Windows form: a
/// `rename` over the target, which is an atomic replace on POSIX.
#[cfg(not(windows))]
fn swap_files(target: &Path, staging: &Path) -> Result<(), SaveError> {
    std::fs::rename(staging, target).map_err(|e| SaveError::new(SaveStep::Swap, e))
}

/// The atomic swap (SPEC.md section 5.1 step 5), Windows form:
/// `ReplaceFileW` with no backup file and
/// `REPLACEFILE_IGNORE_MERGE_ERRORS`, exactly as the spec's sketch shows.
/// `ReplaceFileW` only replaces an *existing* file; when the target does
/// not exist yet the staging file is simply moved onto it (`rename`),
/// which is the same observable result.
#[cfg(windows)]
fn swap_files(target: &Path, staging: &Path) -> Result<(), SaveError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::REPLACEFILE_IGNORE_MERGE_ERRORS;

    if !target.exists() {
        return std::fs::rename(staging, target).map_err(|e| SaveError::new(SaveStep::Swap, e));
    }
    let to_wide = |path: &Path| -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let target_wide = to_wide(target);
    let staging_wide = to_wide(staging);
    // SAFETY: both wide strings are NUL-terminated and outlive the call;
    // the backup, exclude and reserved pointers are null as ReplaceFileW
    // permits. The result is checked before anything relies on it.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::ReplaceFileW(
            target_wide.as_ptr(),
            staging_wide.as_ptr(),
            std::ptr::null(),
            REPLACEFILE_IGNORE_MERGE_ERRORS,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if ok == 0 {
        // SAFETY: GetLastError is a plain thread-local read, called
        // immediately after the failing call.
        let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        return Err(SaveError::new(
            SaveStep::Swap,
            io::Error::from_raw_os_error(code as i32),
        ));
    }
    Ok(())
}

/// Save `table`'s document over `target` atomically (SPEC.md section
/// 5.1 steps 1–5, minus the remap — this function does not touch the
/// table's backing, so it is also the "--save-copy" primitive).
///
/// On any failure before the swap, the staging file is deleted and the
/// original target (if any) is left untouched; the error names the step.
///
/// Note for Windows callers: if `table`'s original backing is a mapping
/// of `target` itself, that mapping must be closed before the swap or
/// `ReplaceFileW` fails. [`PieceTable::save_to`] handles exactly that;
/// this function assumes no mapping of the *target* is held by the
/// table. (A mapping of some *other* file is fine and is how the editor
/// saves a copy of a mapped document.)
pub fn save_atomic(
    table: &PieceTable,
    target: &Path,
    opts: &SaveOptions,
) -> Result<SaveReport, SaveError> {
    if opts.max_chunk == 0 {
        return Err(SaveError::new(
            SaveStep::Staging,
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "max_chunk must be greater than zero",
            ),
        ));
    }
    // Roadmap item 5: refuse early when the disk plainly cannot hold
    // the staging file, instead of failing halfway through the write.
    check_free_space(target, table.len())?;
    let staging = staging_path_for(target);
    let outcome = write_staging(table, &staging, opts)
        .and_then(|written| swap_files(target, &staging).map(|()| written));
    match outcome {
        Ok(written) => Ok(SaveReport {
            bytes_written: written,
            staging_removed: !staging.exists(),
            // save_atomic always streams (the block-clone fast path
            // lives on the in-place save_to_with_options route).
            fast_clone_used: false,
        }),
        Err(err) => {
            // Best-effort cleanup: never leave a staging file behind on
            // failure. The original target has not been touched.
            let _ = std::fs::remove_file(&staging);
            Err(err)
        }
    }
}

impl PieceTable {
    /// Save this table's document over `target` and adopt the saved file
    /// as the table's new original backing (the full SPEC.md section 5.1
    /// pipeline, steps 1–6).
    ///
    /// Steps: stream to staging; close *our own* mapping of the target
    /// (step 4 — a mapped backing is detached and dropped so the swap
    /// can replace the file; memory and length-only backings are simply
    /// held aside, detaching does not invalidate them); atomic swap
    /// (step 5); re-open the saved file as the new backing (step 6 —
    /// re-mapped on Windows, read back into memory on the host) and
    /// reset the table around it: append buffer cleared, pieces reset to
    /// a single `Original` piece. `history` is cleared as part of the
    /// same operation: saving is a history boundary (see the module
    /// docs).
    ///
    /// Failure handling: a failure before the swap leaves the target
    /// byte-identical, removes the staging file, and restores the
    /// table's original backing — held backings are put back verbatim; a
    /// dropped mapping is re-opened from the path it mapped (that file
    /// was not modified, so the bytes are the same). A failure at the
    /// remap step happens *after* the swap succeeded: the file on disk
    /// is saved, the error says so, and the table still resets to the
    /// saved length with a length-only backing (structural edits keep
    /// working; byte reads are unavailable until the file is reopened)
    /// and clears `history`, since the reset invalidates it either way.
    pub fn save_to(
        &mut self,
        target: &Path,
        history: &mut UndoStack,
    ) -> Result<SaveReport, SaveError> {
        self.save_to_with_options(target, history, &SaveOptions::default())
    }

    /// [`PieceTable::save_to`] with explicit [`SaveOptions`].
    pub fn save_to_with_options(
        &mut self,
        target: &Path,
        history: &mut UndoStack,
        opts: &SaveOptions,
    ) -> Result<SaveReport, SaveError> {
        if opts.max_chunk == 0 {
            return Err(SaveError::new(
                SaveStep::Staging,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "max_chunk must be greater than zero",
                ),
            ));
        }
        // Roadmap item 5: the same early disk-space refusal as
        // save_atomic, before the staging file is created.
        check_free_space(target, self.len())?;
        let staging = staging_path_for(target);

        // Block-clone fast save (slice R4c, item 4): when asked
        // for, and only with both transforms Keep (the clone
        // builder assembles raw document bytes), try building the
        // staging file from cloned extents + changed hunks first.
        // The clone source is the file the mapped backing points
        // at; memory/length-only backings have nothing to clone.
        // Any failure quietly falls through to the streamed write.
        let mut fast_clone_used = false;
        let clone_written =
            if opts.fast_clone && opts.eol == EolChoice::Keep && opts.bom == BomChoice::Keep {
                // Peek at the backing's path without disturbing it:
                // detach and immediately put the same buffer back.
                let detached = self.detach_original();
                let source = detached.mapped_path().map(|path| path.to_path_buf());
                self.replace_original(detached);
                match source {
                    Some(source) => write_staging_clone(self, &staging, &source),
                    None => None,
                }
            } else {
                None
            };

        // Steps 1–3: staging file, streaming writes, hardware flush. Our
        // own backing is still attached here: streaming reads from it.
        let written = match clone_written {
            Some(written) => {
                fast_clone_used = true;
                written
            }
            None => match write_staging(self, &staging, opts) {
                Ok(written) => written,
                Err(err) => {
                    let _ = std::fs::remove_file(&staging);
                    return Err(err);
                }
            },
        };

        // Step 4: close our own handlers/mappings to the target before
        // the swap. A mapped backing must actually be *dropped* here —
        // holding it would keep the file mapped and ReplaceFileW would
        // fail; its path is remembered so it can be re-opened if the
        // swap fails. Memory and length-only backings are held aside in
        // `held`: dropping them would destroy bytes that exist nowhere
        // else.
        let detached = self.detach_original();
        let mapped_path = detached.mapped_path().map(|path| path.to_path_buf());
        let held: Option<OriginalBuffer> = if mapped_path.is_some() {
            drop(detached);
            None
        } else {
            Some(detached)
        };

        // Step 5: the atomic swap.
        if let Err(err) = swap_files(target, &staging) {
            let _ = std::fs::remove_file(&staging);
            // The target is untouched: restore the original backing so
            // the table reads exactly what it read before the save.
            match held {
                Some(backing) => self.replace_original(backing),
                None => {
                    let path = mapped_path.as_deref().expect("mapped backing has a path");
                    match reopen_original(path) {
                        Ok(backing) => self.replace_original(backing),
                        Err(_) => self.replace_original(OriginalBuffer::LengthOnly {
                            len: self.original_file_size,
                        }),
                    }
                }
            }
            return Err(err);
        }

        // The old backing is obsolete now, whatever it was.
        drop(held);

        // Step 6: adopt the saved file as the new original backing.
        match open_saved_backing(target, written) {
            Ok(backing) => {
                self.reset_to_original(backing);
                history.clear();
                Ok(SaveReport {
                    bytes_written: written,
                    staging_removed: !staging.exists(),
                    fast_clone_used,
                })
            }
            Err(source) => {
                // The save itself succeeded — only the re-open failed.
                // Keep the table structurally consistent with the saved
                // file (correct length, no append buffer) and clear the
                // history the reset invalidates, but surface the failure
                // honestly.
                self.reset_to_original(OriginalBuffer::LengthOnly { len: written });
                history.clear();
                Err(SaveError::new(SaveStep::Remap, source))
            }
        }
    }
}

/// Re-open the file a dropped mapped backing pointed at, after a failed
/// swap: re-mapped on Windows, read into memory on the host (where a
/// mapped backing cannot exist, so this is only a completeness path).
#[cfg(windows)]
fn reopen_original(path: &Path) -> io::Result<OriginalBuffer> {
    let mmap = crate::mmap_win::MmapFile::open(path)?;
    Ok(OriginalBuffer::Mapped(std::sync::Arc::new(mmap)))
}

/// Re-open the file a dropped mapped backing pointed at, after a failed
/// swap (host form; see the Windows version).
#[cfg(not(windows))]
fn reopen_original(path: &Path) -> io::Result<OriginalBuffer> {
    let bytes = std::fs::read(path)?;
    Ok(OriginalBuffer::Memory(bytes))
}

/// Open the just-saved file as a fresh original backing: re-mapped on
/// Windows (step 6 "remap"), read back into memory on the host.
#[cfg(windows)]
fn open_saved_backing(target: &Path, _written: u64) -> io::Result<OriginalBuffer> {
    let mmap = crate::mmap_win::MmapFile::open(target)?;
    Ok(OriginalBuffer::Mapped(std::sync::Arc::new(mmap)))
}

/// Open the just-saved file as a fresh original backing: read back into
/// memory on the host (Windows re-maps instead — see above).
#[cfg(not(windows))]
fn open_saved_backing(target: &Path, _written: u64) -> io::Result<OriginalBuffer> {
    let bytes = std::fs::read(target)?;
    Ok(OriginalBuffer::Memory(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::undo::UndoStack;

    /// A unique empty temp directory per test, removed by the caller.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("veloedit-save-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An edited, multi-piece document: "hello world" with an insert and
    /// a delete, expected content "hello new world!".
    fn edited_table() -> PieceTable {
        let mut table = PieceTable::from_original(b"hello world".to_vec());
        table.insert(5, b" new");
        table.insert(15, b"!");
        assert_eq!(table.read(0, 99), b"hello new world!");
        assert!(table.piece_count() > 1);
        table
    }

    // (f) The staging filename follows the section 5.1 pattern.
    #[test]
    fn staging_name_matches_spec_pattern() {
        let target = Path::new("/some/dir/Filename.ext");
        let staging = staging_path_for(target);
        assert_eq!(staging.parent(), Some(Path::new("/some/dir")));
        let name = staging.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            name,
            format!("Filename.ext.tmp_velo_{}", std::process::id())
        );
    }

    // (R4c, item 33) The read-only attribute bit test.
    #[test]
    fn attr_read_only_bit_is_the_windows_value() {
        assert!(attr_is_read_only(FILE_ATTRIBUTE_READONLY_BIT));
        assert!(attr_is_read_only(0x0000_0001 | 0x0000_0020)); // + archive
        assert!(!attr_is_read_only(0));
        assert!(!attr_is_read_only(0x0000_0020)); // archive only
        assert!(!attr_is_read_only(0x0000_0010)); // directory only
    }

    // (R4c, item 33) A real file flagged read-only reports as
    // such; a normal file and a missing file do not.
    // (The allow: restoring writability so the temp dir can be
    // removed is the point of the second set_readonly call;
    // clippy's Unix world-writable concern does not apply to a
    // test fixture that is deleted immediately after.)
    #[allow(clippy::permissions_set_readonly_false)]
    #[test]
    fn path_read_only_follows_the_files_flag() {
        let dir = temp_dir("readonly-flag");
        let target = dir.join("guarded.txt");
        std::fs::write(&target, b"content").unwrap();
        assert!(!path_is_read_only(&target));
        let mut perms = std::fs::metadata(&target).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&target, perms).unwrap();
        assert!(path_is_read_only(&target));
        // Restore writability so the temp dir cleans up everywhere.
        let mut perms = std::fs::metadata(&target).unwrap().permissions();
        perms.set_readonly(false);
        std::fs::set_permissions(&target, perms).unwrap();
        assert!(!path_is_read_only(&target));
        assert!(!path_is_read_only(&dir.join("missing.txt")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // (R4c, item 4) With fast_clone requested, a save still
    // produces the exact document bytes — on the host (and for a
    // memory-backed table anywhere) the clone path declines and
    // the normal streamed save runs, reporting fast_clone_used =
    // false. The Windows clone success path needs a mapped
    // backing on a clone-capable volume, so it is exercised by
    // field testing, not here.
    #[test]
    fn fast_clone_request_falls_back_to_a_correct_save() {
        let dir = temp_dir("fast-clone-fallback");
        let target = dir.join("doc.txt");
        std::fs::write(&target, b"hello world").unwrap();
        let mut table = edited_table();
        let mut history = UndoStack::new();
        let opts = SaveOptions {
            fast_clone: true,
            ..SaveOptions::default()
        };
        let report = table
            .save_to_with_options(&target, &mut history, &opts)
            .unwrap();
        assert_eq!(report.bytes_written, 16);
        assert!(!report.fast_clone_used);
        assert_eq!(std::fs::read(&target).unwrap(), b"hello new world!");
        // After save_to_with_options the table's backing is a live
        // mapping of the target on Windows; drop it before deleting
        // the directory or Windows refuses with
        // ERROR_SHARING_VIOLATION (the same rule the save_to tests
        // below follow).
        drop(table);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // (a) Save an edited multi-piece document; the saved bytes are the
    // edited document.
    #[test]
    fn save_atomic_writes_edited_document() {
        let dir = temp_dir("atomic-basic");
        let target = dir.join("doc.txt");
        let table = edited_table();
        let report = save_atomic(&table, &target, &SaveOptions::default()).unwrap();
        assert_eq!(report.bytes_written, 16);
        assert!(report.staging_removed);
        assert!(!staging_path_for(&target).exists());
        assert_eq!(std::fs::read(&target).unwrap(), b"hello new world!");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // Chunked streaming: with a tiny chunk cap the writes are chopped but
    // the saved content is unchanged.
    #[test]
    fn save_atomic_chunks_large_pieces() {
        let dir = temp_dir("atomic-chunks");
        let target = dir.join("big.bin");
        let table = PieceTable::from_original(vec![b'x'; 100_000]);
        let opts = SaveOptions {
            max_chunk: 999,
            ..SaveOptions::default()
        };
        let report = save_atomic(&table, &target, &opts).unwrap();
        assert_eq!(report.bytes_written, 100_000);
        assert_eq!(std::fs::read(&target).unwrap().len(), 100_000);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // (b) Saving over an existing file replaces it.
    #[test]
    fn save_atomic_replaces_existing_file() {
        let dir = temp_dir("atomic-replace");
        let target = dir.join("doc.txt");
        std::fs::write(&target, b"old content that is much longer").unwrap();
        let table = edited_table();
        save_atomic(&table, &target, &SaveOptions::default()).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"hello new world!");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // (c) A zero-length document saves as a zero-length file.
    #[test]
    fn save_atomic_zero_length_document() {
        let dir = temp_dir("atomic-empty");
        let target = dir.join("empty.bin");
        let table = PieceTable::from_original(Vec::new());
        let report = save_atomic(&table, &target, &SaveOptions::default()).unwrap();
        assert_eq!(report.bytes_written, 0);
        assert!(report.staging_removed);
        assert_eq!(std::fs::read(&target).unwrap(), Vec::<u8>::new());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // (d1) Failure creating the staging file (target inside a directory
    // that does not exist): error at the staging step, no staging file
    // left behind, nothing created.
    #[test]
    fn failed_staging_leaves_nothing_behind() {
        let dir = temp_dir("fail-staging");
        let target = dir.join("no-such-dir").join("doc.txt");
        let table = edited_table();
        let err = save_atomic(&table, &target, &SaveOptions::default()).unwrap_err();
        assert_eq!(err.step, SaveStep::Staging);
        assert!(!staging_path_for(&target).exists());
        assert!(!dir.join("no-such-dir").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // (d2) Failure at the swap step (the target is a non-empty
    // directory): the original is untouched and the staging file is gone.
    #[test]
    fn failed_swap_preserves_target_and_removes_staging() {
        let dir = temp_dir("fail-swap");
        let target = dir.join("a-directory");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("keep.txt"), b"precious").unwrap();
        let table = edited_table();
        let err = save_atomic(&table, &target, &SaveOptions::default()).unwrap_err();
        assert_eq!(err.step, SaveStep::Swap);
        assert_eq!(std::fs::read(target.join("keep.txt")).unwrap(), b"precious");
        assert!(!staging_path_for(&target).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // (a) via save_to: after saving, the bytes on disk are the document.
    #[test]
    fn save_to_writes_document_to_disk() {
        let dir = temp_dir("to-disk");
        let target = dir.join("doc.txt");
        std::fs::write(&target, b"hello world").unwrap();
        let mut table = PieceTable::from_original(b"hello world".to_vec());
        let mut history = UndoStack::new();
        history.do_insert(&mut table, 5, b" new");
        let report = table.save_to(&target, &mut history).unwrap();
        assert_eq!(report.bytes_written, 15);
        assert_eq!(std::fs::read(&target).unwrap(), b"hello new world");
        // After save_to the table's backing is a live mapping of the
        // target on Windows; drop it before deleting the directory or
        // Windows refuses with ERROR_SHARING_VIOLATION.
        drop(table);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // (e) After save_to: the table reads equal the saved content, the
    // append buffer is empty, the piece list is one Original piece, and
    // the undo stack is empty (save is a history boundary).
    #[test]
    fn save_to_resets_table_and_clears_history() {
        let dir = temp_dir("to-reset");
        let target = dir.join("doc.txt");
        let mut table = edited_table();
        let mut history = UndoStack::new();
        history.do_insert(&mut table, 0, b">> ");
        assert!(history.can_undo());
        let expected = table.read(0, table.len());
        table.save_to(&target, &mut history).unwrap();
        assert_eq!(table.read(0, table.len()), expected);
        assert_eq!(std::fs::read(&target).unwrap(), expected);
        assert!(table.append_buffer.is_empty());
        assert_eq!(table.piece_count(), 1);
        assert_eq!(
            table.pieces[0].source,
            crate::piece_table::BufferSource::Original
        );
        assert!(!history.can_undo());
        assert!(!history.can_redo());
        assert!(!history.apply_undo(&mut table));
        assert!(table.check_invariants());
        // And the table keeps working afterwards.
        table.insert(0, b"again ");
        assert_eq!(table.read(0, 99), b"again >> hello new world!");
        // Drop the table (and its live target mapping) before deleting.
        drop(table);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // (e) Zero-length save_to: empty document, empty file, empty table.
    #[test]
    fn save_to_zero_length_document() {
        let dir = temp_dir("to-empty");
        let target = dir.join("empty.bin");
        let mut table = PieceTable::from_original(Vec::new());
        let mut history = UndoStack::new();
        let report = table.save_to(&target, &mut history).unwrap();
        assert_eq!(report.bytes_written, 0);
        assert_eq!(table.len(), 0);
        assert_eq!(table.piece_count(), 0);
        assert!(table.append_buffer.is_empty());
        assert_eq!(std::fs::read(&target).unwrap(), Vec::<u8>::new());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // save_to failure before the swap: the table must be byte-for-byte
    // and structurally unchanged afterwards (backing restored).
    #[test]
    fn save_to_failed_swap_leaves_table_unchanged() {
        let dir = temp_dir("to-fail-swap");
        let target = dir.join("a-directory");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("keep.txt"), b"precious").unwrap();
        let mut table = edited_table();
        let pieces_before = table.pieces.clone();
        let append_before = table.append_buffer.clone();
        let mut history = UndoStack::new();
        let err = table.save_to(&target, &mut history).unwrap_err();
        assert_eq!(err.step, SaveStep::Swap);
        assert_eq!(table.read(0, table.len()), b"hello new world!");
        assert_eq!(table.pieces, pieces_before);
        assert_eq!(table.append_buffer, append_before);
        assert!(table.check_invariants());
        assert!(!staging_path_for(&target).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // (item 5) The space requirement is the length plus a twentieth,
    // saturating at the top of the range instead of overflowing.
    #[test]
    fn space_needed_is_five_percent_over() {
        assert_eq!(space_needed(0), 0);
        assert_eq!(space_needed(19), 19);
        assert_eq!(space_needed(20), 21);
        assert_eq!(space_needed(100), 105);
        assert_eq!(space_needed(1_000_000), 1_050_000);
        assert_eq!(space_needed(u64::MAX), u64::MAX);
    }

    // (items 35/44) Range clamping: inside, overhanging, at the end,
    // and beyond the end.
    #[test]
    fn clamp_range_cases() {
        assert_eq!(clamp_range(0, 10, 100), Some((0, 10)));
        assert_eq!(clamp_range(90, 50, 100), Some((90, 10)));
        assert_eq!(clamp_range(100, 50, 100), Some((100, 0)));
        assert_eq!(clamp_range(101, 50, 100), None);
        assert_eq!(clamp_range(0, 0, 0), Some((0, 0)));
        assert_eq!(clamp_range(1, 0, 0), None);
    }

    // (items 35/44) The range writer streams exactly the document
    // bytes of the (clamped) range, across piece boundaries.
    #[test]
    fn write_table_range_round_trip() {
        let table = edited_table(); // "hello new world!", multi-piece
        let mut out: Vec<u8> = Vec::new();
        let written = write_table_range(&table, 3, 8, &mut out).unwrap();
        assert_eq!(written, 8);
        assert_eq!(out, b"lo new w");
        // An overhanging length clamps to the document end.
        let mut out: Vec<u8> = Vec::new();
        let written = write_table_range(&table, 10, 999, &mut out).unwrap();
        assert_eq!(written, 6);
        assert_eq!(out, b"world!");
        // A start beyond the end is an error and writes nothing.
        let mut out: Vec<u8> = Vec::new();
        assert!(write_table_range(&table, 17, 1, &mut out).is_err());
        assert!(out.is_empty());
    }

    // (item 35) The file form writes the range to disk, and a bad
    // range leaves an existing destination file untouched.
    #[test]
    fn write_range_to_file_round_trip() {
        let dir = temp_dir("range-file");
        let dest = dir.join("slice.bin");
        let table = edited_table();
        let written = write_range_to_file(&table, 6, 3, &dest).unwrap();
        assert_eq!(written, 3);
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        // Beyond-the-end start: error, and the file keeps its bytes.
        assert!(write_range_to_file(&table, 100, 1, &dest).is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // (item 36) Split names: three digits minimum, growing past 999.
    #[test]
    fn split_part_names_are_zero_padded_and_sortable() {
        assert_eq!(split_part_name("big.bin", 1, 3), "big.bin.part001");
        assert_eq!(split_part_name("big.bin", 12, 300), "big.bin.part012");
        assert_eq!(
            split_part_name("big.bin", 10_000, 10_000),
            "big.bin.part10000"
        );
        assert_eq!(split_part_count(0, 5), 1);
        assert_eq!(split_part_count(10, 5), 2);
        assert_eq!(split_part_count(11, 5), 3);
    }

    // (item 36) Splitting streams the source into ordered parts whose
    // join is the source byte for byte, last part short.
    #[test]
    fn split_file_round_trip() {
        let dir = temp_dir("split-basic");
        let src = dir.join("data.bin");
        let data: Vec<u8> = (0u8..=255).cycle().take(1030).collect();
        std::fs::write(&src, &data).unwrap();
        let out = dir.join("parts");
        let paths = split_file(&src, 100, &out).unwrap();
        assert_eq!(paths.len(), 11); // ceil(1030 / 100)
        assert_eq!(
            paths[0].file_name().unwrap().to_string_lossy(),
            "data.bin.part001"
        );
        assert_eq!(
            paths[10].file_name().unwrap().to_string_lossy(),
            "data.bin.part011"
        );
        let mut joined: Vec<u8> = Vec::new();
        for (i, path) in paths.iter().enumerate() {
            let bytes = std::fs::read(path).unwrap();
            assert_eq!(bytes.len(), if i < 10 { 100 } else { 30 });
            joined.extend_from_slice(&bytes);
        }
        assert_eq!(joined, data);
        // The source is untouched.
        assert_eq!(std::fs::read(&src).unwrap(), data);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // (item 36) An exact multiple makes no trailing empty part, and an
    // empty source makes exactly one empty part.
    #[test]
    fn split_file_exact_multiple_and_empty_source() {
        let dir = temp_dir("split-edges");
        let src = dir.join("even.bin");
        std::fs::write(&src, vec![7u8; 300]).unwrap();
        let paths = split_file(&src, 100, &dir.join("a")).unwrap();
        assert_eq!(paths.len(), 3);
        let empty = dir.join("empty.bin");
        std::fs::write(&empty, b"").unwrap();
        let paths = split_file(&empty, 100, &dir.join("b")).unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(std::fs::read(&paths[0]).unwrap(), Vec::<u8>::new());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // (item 36) Refusals happen before anything is written: a zero
    // part size, and a part count past the limit.
    #[test]
    fn split_file_refusals_write_nothing() {
        let dir = temp_dir("split-refuse");
        let src = dir.join("many.bin");
        std::fs::write(&src, vec![1u8; 20_000]).unwrap();
        let out = dir.join("parts");
        let err = split_file(&src, 0, &out).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        // 20,000 one-byte parts is past the 10,000 limit.
        let err = split_file(&src, 1, &out).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!out.exists(), "a refused split creates nothing");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn zero_max_chunk_is_an_error_not_a_hang() {
        let dir = temp_dir("bad-chunk");
        let target = dir.join("doc.txt");
        let table = edited_table();
        let opts = SaveOptions {
            max_chunk: 0,
            ..SaveOptions::default()
        };
        let err = save_atomic(&table, &target, &opts).unwrap_err();
        assert_eq!(err.step, SaveStep::Staging);
        assert!(!target.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ---- Save transforms (roadmap items 31/32) ----

    /// Save `table` through `save_atomic` with `opts` into a fresh
    /// temp file and return the bytes that landed on disk.
    fn saved_bytes(tag: &str, table: &PieceTable, opts: &SaveOptions) -> Vec<u8> {
        let dir = temp_dir(tag);
        let target = dir.join("doc.bin");
        let report = save_atomic(table, &target, opts).unwrap();
        let bytes = std::fs::read(&target).unwrap();
        assert_eq!(report.bytes_written, bytes.len() as u64);
        std::fs::remove_dir_all(&dir).unwrap();
        bytes
    }

    fn transform_opts(eol: EolChoice, bom: BomChoice, encoding: TextEncoding) -> SaveOptions {
        SaveOptions {
            encoding,
            eol,
            bom,
            ..SaveOptions::default()
        }
    }

    // CRLF → LF with a chunk size that splits a CRLF pair across
    // two chunks ("ab\r" | "\ncd"…): the held CR must not become
    // its own line ending.
    #[test]
    fn eol_crlf_to_lf_with_pair_straddling_chunks() {
        let table = PieceTable::from_original(b"ab\r\ncd\r\n".to_vec());
        let mut opts = transform_opts(EolChoice::Lf, BomChoice::Keep, TextEncoding::Utf8);
        opts.max_chunk = 3;
        assert_eq!(saved_bytes("eol-straddle", &table, &opts), b"ab\ncd\n");
        // Every chunk size from 1 up gives the same result.
        for chunk in 1..=8usize {
            opts.max_chunk = chunk;
            assert_eq!(
                saved_bytes("eol-straddle-n", &table, &opts),
                b"ab\ncd\n",
                "chunk {chunk}"
            );
        }
    }

    // A CRLF pair split across *pieces* (CR ends one piece, LF
    // starts the next) converts as one ending too.
    #[test]
    fn eol_conversion_across_a_piece_boundary() {
        let mut table = PieceTable::from_original(b"a\r".to_vec());
        table.insert(2, b"\nb");
        assert_eq!(table.read(0, table.len()), b"a\r\nb");
        let opts = transform_opts(EolChoice::Lf, BomChoice::Keep, TextEncoding::Utf8);
        assert_eq!(saved_bytes("eol-pieces", &table, &opts), b"a\nb");
    }

    // Mixed endings normalise in both directions, and a trailing
    // lone CR still counts as an ending.
    #[test]
    fn eol_normalises_mixed_endings_both_ways() {
        let table = PieceTable::from_original(b"a\r\nb\nc\rd".to_vec());
        let lf = transform_opts(EolChoice::Lf, BomChoice::Keep, TextEncoding::Utf8);
        assert_eq!(saved_bytes("eol-mixed-lf", &table, &lf), b"a\nb\nc\nd");
        let crlf = transform_opts(EolChoice::Crlf, BomChoice::Keep, TextEncoding::Utf8);
        assert_eq!(
            saved_bytes("eol-mixed-crlf", &table, &crlf),
            b"a\r\nb\r\nc\r\nd"
        );
        let trailing = PieceTable::from_original(b"end\r".to_vec());
        assert_eq!(saved_bytes("eol-trail-lf", &trailing, &lf), b"end\n");
        assert_eq!(saved_bytes("eol-trail-crlf", &trailing, &crlf), b"end\r\n");
    }

    // UTF-16LE: conversion works on 2-byte units, with the BOM unit
    // passing through as an ordinary unit under BomChoice::Keep.
    #[test]
    fn eol_conversion_utf16le() {
        let mut bytes = vec![0xFF, 0xFE]; // BOM
        for unit in [0x0061u16, 0x000D, 0x000A, 0x0062, 0x000A, 0x0063] {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let table = PieceTable::from_original(bytes);
        let opts = transform_opts(EolChoice::Lf, BomChoice::Keep, TextEncoding::Utf16Le);
        let mut want = vec![0xFF, 0xFE];
        for unit in [0x0061u16, 0x000A, 0x0062, 0x000A, 0x0063] {
            want.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(saved_bytes("eol-u16le", &table, &opts), want);
    }

    // UTF-16BE: LF → CRLF, byte order respected; chunks of 3 bytes
    // split units across feeds.
    #[test]
    fn eol_conversion_utf16be_odd_chunks() {
        let mut bytes = Vec::new();
        for unit in [0x0061u16, 0x000A, 0x0062] {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
        let table = PieceTable::from_original(bytes);
        let mut opts = transform_opts(EolChoice::Crlf, BomChoice::Keep, TextEncoding::Utf16Be);
        opts.max_chunk = 3;
        let mut want = Vec::new();
        for unit in [0x0061u16, 0x000D, 0x000A, 0x0062] {
            want.extend_from_slice(&unit.to_be_bytes());
        }
        assert_eq!(saved_bytes("eol-u16be", &table, &opts), want);
    }

    // BOM add: prepended when missing, never doubled when present.
    // BOM remove: stripped when present, a no-op otherwise.
    #[test]
    fn bom_add_and_remove_utf8() {
        let plain = PieceTable::from_original(b"abc".to_vec());
        let with_bom = PieceTable::from_original(b"\xEF\xBB\xBFabc".to_vec());
        let add = transform_opts(EolChoice::Keep, BomChoice::Add, TextEncoding::Utf8);
        assert_eq!(saved_bytes("bom-add", &plain, &add), b"\xEF\xBB\xBFabc");
        assert_eq!(
            saved_bytes("bom-add-present", &with_bom, &add),
            b"\xEF\xBB\xBFabc"
        );
        let remove = transform_opts(EolChoice::Keep, BomChoice::Remove, TextEncoding::Utf8);
        assert_eq!(saved_bytes("bom-remove", &with_bom, &remove), b"abc");
        assert_eq!(saved_bytes("bom-remove-absent", &plain, &remove), b"abc");
    }

    // BOM add/remove for UTF-16 (2-byte marks), and ANSI — which
    // has no BOM — treats both as no-ops.
    #[test]
    fn bom_add_and_remove_utf16_and_ansi() {
        let mut le = vec![0xFF, 0xFE];
        le.extend_from_slice(&0x0061u16.to_le_bytes());
        let table = PieceTable::from_original(le);
        let remove = transform_opts(EolChoice::Keep, BomChoice::Remove, TextEncoding::Utf16Le);
        assert_eq!(
            saved_bytes("bom-u16-remove", &table, &remove),
            0x0061u16.to_le_bytes()
        );
        let add = transform_opts(EolChoice::Keep, BomChoice::Add, TextEncoding::Utf16Le);
        let bare = PieceTable::from_original(0x0061u16.to_le_bytes().to_vec());
        let mut want = vec![0xFF, 0xFE];
        want.extend_from_slice(&0x0061u16.to_le_bytes());
        assert_eq!(saved_bytes("bom-u16-add", &bare, &add), want);

        let ansi = PieceTable::from_original(b"abc".to_vec());
        let add_ansi = transform_opts(EolChoice::Keep, BomChoice::Add, TextEncoding::Ansi);
        assert_eq!(saved_bytes("bom-ansi", &ansi, &add_ansi), b"abc");
    }

    // Adding a BOM to an empty document writes just the mark;
    // Keep/Keep stays an identity even with transforms configured
    // in the options' other fields.
    #[test]
    fn bom_add_to_empty_document_and_keep_identity() {
        let empty = PieceTable::from_original(Vec::new());
        let add = transform_opts(EolChoice::Keep, BomChoice::Add, TextEncoding::Utf8);
        assert_eq!(saved_bytes("bom-empty", &empty, &add), b"\xEF\xBB\xBF");

        let table = PieceTable::from_original(b"\xEF\xBB\xBFa\r\nb\r".to_vec());
        let keep = SaveOptions::default();
        assert_eq!(
            saved_bytes("keep-identity", &table, &keep),
            b"\xEF\xBB\xBFa\r\nb\r"
        );
    }

    // BOM removal and EOL conversion compose: the mark goes, the
    // endings convert.
    #[test]
    fn bom_and_eol_transforms_compose() {
        let table = PieceTable::from_original(b"\xEF\xBB\xBFa\r\nb".to_vec());
        let opts = transform_opts(EolChoice::Lf, BomChoice::Remove, TextEncoding::Utf8);
        assert_eq!(saved_bytes("bom-eol", &table, &opts), b"a\nb");
    }
}
