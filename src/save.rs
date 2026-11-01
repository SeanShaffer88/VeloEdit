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

use crate::piece_table::{OriginalBuffer, PieceTable};
use crate::undo::UndoStack;

/// Default streaming chunk size: the 4 MB blocks of SPEC.md section 5.1.
pub const DEFAULT_MAX_CHUNK: usize = 4 * 1024 * 1024;

/// Options for the save operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SaveOptions {
    /// Largest single write issued while streaming the staging file.
    /// Must be greater than zero.
    pub max_chunk: usize,
}

impl Default for SaveOptions {
    fn default() -> Self {
        Self {
            max_chunk: DEFAULT_MAX_CHUNK,
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

/// Stream the whole document into the staging file at `staging` in
/// chunks of at most `max_chunk` bytes, flush it to disk, and close it.
/// Returns the number of bytes written.
fn write_staging(table: &PieceTable, staging: &Path, max_chunk: usize) -> Result<u64, SaveError> {
    debug_assert!(max_chunk > 0);
    let mut file = File::create(staging).map_err(|e| SaveError::new(SaveStep::Staging, e))?;
    let mut written: u64 = 0;
    for slice in table.iter_slices() {
        for chunk in slice.chunks(max_chunk) {
            file.write_all(chunk)
                .map_err(|e| SaveError::new(SaveStep::Write, e))?;
            written += chunk.len() as u64;
        }
    }
    flush_file(&file).map_err(|e| SaveError::new(SaveStep::Flush, e))?;
    // Close before the swap (the File drops here): ReplaceFileW cannot
    // replace from a file that is still open for writing.
    drop(file);
    Ok(written)
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
    let staging = staging_path_for(target);
    let outcome = write_staging(table, &staging, opts.max_chunk)
        .and_then(|written| swap_files(target, &staging).map(|()| written));
    match outcome {
        Ok(written) => Ok(SaveReport {
            bytes_written: written,
            staging_removed: !staging.exists(),
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
        let staging = staging_path_for(target);

        // Steps 1–3: staging file, streaming writes, hardware flush. Our
        // own backing is still attached here: streaming reads from it.
        let written = match write_staging(self, &staging, opts.max_chunk) {
            Ok(written) => written,
            Err(err) => {
                let _ = std::fs::remove_file(&staging);
                return Err(err);
            }
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
        let opts = SaveOptions { max_chunk: 999 };
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

    #[test]
    fn zero_max_chunk_is_an_error_not_a_hang() {
        let dir = temp_dir("bad-chunk");
        let target = dir.join("doc.txt");
        let table = edited_table();
        let opts = SaveOptions { max_chunk: 0 };
        let err = save_atomic(&table, &target, &opts).unwrap_err();
        assert_eq!(err.step, SaveStep::Staging);
        assert!(!target.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
