//! Windows read-only memory-mapped file backing (SPEC.md section 2.1).
//!
//! This module is the ONLY place in the crate where `unsafe` Windows FFI
//! for file mapping lives (see `PROJECT_RULES.md`, unsafe confinement
//! rule). It wraps `CreateFileW` / `CreateFileMappingW` / `MapViewOfFile`
//! in an RAII type: the OS loads pages lazily via hardware faults, so
//! opening even a 100+ GB file costs no upfront disk read and almost no
//! RAM — which is the whole point of the piece-table design.
//!
//! Soundness note for [`MmapFile::as_slice`]: the view is created with
//! `PAGE_READONLY` from a handle opened `GENERIC_READ`, so the bytes
//! cannot be written through this mapping, and the mapping keeps the file
//! object alive for as long as the view exists. One residual risk is
//! named honestly: the file is opened with `FILE_SHARE_READ |
//! FILE_SHARE_WRITE`, so another process can still open the file for
//! writing and *truncate* it. If the file shrinks below a page the reader
//! later touches, that read faults with an access violation instead of
//! returning data. This is the standard trade of the blueprint's design
//! (SPEC.md section 2.1 opens with share modes that do not lock other
//! writers out), and it is recorded as a known limitation in `QUALITY.md`
//! rather than hidden.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileSizeEx, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS,
    PAGE_READONLY,
};

/// Which step of opening a mapping failed. Named in every error so a
/// failure report says *what* went wrong, not just an OS code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MmapStep {
    Open,
    Size,
    Mapping,
    View,
}

impl std::fmt::Display for MmapStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            MmapStep::Open => "open",
            MmapStep::Size => "size",
            MmapStep::Mapping => "mapping",
            MmapStep::View => "view",
        };
        f.write_str(name)
    }
}

/// Error opening a memory-mapped file: the failed step plus the OS error.
#[derive(Debug)]
pub struct MmapError {
    pub step: MmapStep,
    pub source: io::Error,
}

impl std::fmt::Display for MmapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "memory-mapping failed at the {} step: {}",
            self.step, self.source
        )
    }
}

impl std::error::Error for MmapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl From<MmapError> for io::Error {
    fn from(err: MmapError) -> Self {
        io::Error::new(err.source.kind(), err.to_string())
    }
}

fn step_error(step: MmapStep) -> MmapError {
    // SAFETY: GetLastError is a plain thread-local read with no preconditions;
    // it is called immediately after the failing call, before anything can
    // overwrite the code.
    let code = unsafe { GetLastError() };
    MmapError {
        step,
        source: io::Error::from_raw_os_error(code as i32),
    }
}

/// The raw mapped view plus the two handles that own it. Dropped exactly
/// once (see the `Drop` impl): unmap the view, close the mapping handle,
/// close the file handle — the order SPEC.md section 5.1 step 4 requires.
///
/// The struct is public only because [`MmapFile::Mapped`] exposes it;
/// its fields are private and it has no public constructor, so a view
/// can only ever come from [`MmapFile::open`].
pub struct MappedView {
    path: std::path::PathBuf,
    file: HANDLE,
    mapping: HANDLE,
    view: *mut c_void,
    len: u64,
}

// Manual Debug: the raw handles/view pointer are not useful in a debug
// print (and HANDLE's Debug availability varies by windows-sys version),
// so only the meaningful state is shown.
impl std::fmt::Debug for MappedView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedView")
            .field("path", &self.path)
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

// SAFETY: the view is read-only (PAGE_READONLY) and is never mutated
// through this type; sharing it between threads only ever produces
// concurrent reads of the same immutable bytes. Moving it between threads
// moves ownership of the handles, and Drop runs on whichever thread owns
// it last — UnmapViewOfFile/CloseHandle are safe to call from any thread.
unsafe impl Send for MappedView {}
// SAFETY: as above — `&MappedView` grants only `as_slice` reads of
// immutable, read-only mapped bytes.
unsafe impl Sync for MappedView {}

impl Drop for MappedView {
    fn drop(&mut self) {
        // SAFETY: `view` came from MapViewOfFile for exactly this mapping
        // and has not been unmapped (Drop runs once); UnmapViewOfFile is
        // the documented release for it. The handles are valid, owned by
        // this value, and each is closed exactly here, exactly once.
        unsafe {
            let address = MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view };
            UnmapViewOfFile(address);
            CloseHandle(self.mapping);
            CloseHandle(self.file);
        }
    }
}

/// A read-only memory-mapped file (SPEC.md section 2.1).
///
/// Zero-length files are the dedicated [`MmapFile::Empty`] variant:
/// `CreateFileMappingW` fails for a 0-byte file, so it is never called
/// for one.
#[derive(Debug)]
pub enum MmapFile {
    Empty,
    Mapped(MappedView),
}

impl MmapFile {
    /// Open `path` and map the whole file read-only.
    ///
    /// The file is opened with `GENERIC_READ` and share modes
    /// `FILE_SHARE_READ | FILE_SHARE_WRITE` (SPEC.md section 2.1 names
    /// `FILE_SHARE_READ`; write sharing is added so the atomic save's
    /// `ReplaceFileW` can replace a file this process is viewing — the
    /// save pipeline closes this mapping before swapping, section 5.1
    /// step 4). Errors name the failing step (open / size / mapping /
    /// view).
    pub fn open(path: &Path) -> Result<Self, MmapError> {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the
        // call; the remaining arguments are plain values / null pointers
        // permitted by CreateFileW's contract. The returned handle is
        // checked against INVALID_HANDLE_VALUE before any use.
        let file = unsafe {
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
        if file == INVALID_HANDLE_VALUE {
            return Err(step_error(MmapStep::Open));
        }

        let mut size: i64 = 0;
        // SAFETY: `file` is a valid open handle from the check above and
        // `size` is a writable i64 that outlives the call.
        let ok = unsafe { GetFileSizeEx(file, &mut size) };
        if ok == 0 {
            let err = step_error(MmapStep::Size);
            // SAFETY: `file` is valid and owned by us; closed exactly once
            // on this error path (ownership never reached a MappedView).
            unsafe { CloseHandle(file) };
            return Err(err);
        }
        if size < 0 {
            // SAFETY: as above, the error path closes the handle once.
            unsafe { CloseHandle(file) };
            return Err(MmapError {
                step: MmapStep::Size,
                source: io::Error::new(io::ErrorKind::InvalidData, "file size was negative"),
            });
        }
        let len = size as u64;
        if len == 0 {
            // Never call CreateFileMappingW for a zero-length file: it
            // fails with ERROR_FILE_INVALID. Close the handle and report
            // the dedicated Empty variant instead.
            // SAFETY: `file` is valid and owned by us; closed exactly once.
            unsafe { CloseHandle(file) };
            return Ok(MmapFile::Empty);
        }

        // SAFETY: `file` is a valid handle to a non-empty file opened with
        // GENERIC_READ, which permits a PAGE_READONLY mapping; size 0/0
        // means "map the whole file". The returned handle is null-checked
        // before use.
        let mapping = unsafe {
            CreateFileMappingW(
                file,
                std::ptr::null(),
                PAGE_READONLY,
                0,
                0,
                std::ptr::null(),
            )
        };
        if mapping.is_null() {
            let err = step_error(MmapStep::Mapping);
            // SAFETY: `file` is valid and owned by us; closed exactly once
            // on this error path.
            unsafe { CloseHandle(file) };
            return Err(err);
        }

        // SAFETY: `mapping` is a valid mapping handle from the check
        // above; offset 0 and size 0 map the whole file read-only. The
        // returned view pointer is null-checked before it is stored.
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0) };
        if view.Value.is_null() {
            let err = step_error(MmapStep::View);
            // SAFETY: both handles are valid and owned by us; each is
            // closed exactly once on this error path.
            unsafe {
                CloseHandle(mapping);
                CloseHandle(file);
            }
            return Err(err);
        }

        Ok(MmapFile::Mapped(MappedView {
            path: path.to_path_buf(),
            file,
            mapping,
            view: view.Value,
            len,
        }))
    }

    /// The path this mapping was opened from (`None` for the empty
    /// variant). The save pipeline uses it to re-open the mapping after
    /// closing it for an atomic swap.
    pub fn path(&self) -> Option<&Path> {
        match self {
            MmapFile::Empty => None,
            MmapFile::Mapped(view) => Some(&view.path),
        }
    }

    /// Length of the mapped file in bytes.
    pub fn len(&self) -> u64 {
        match self {
            MmapFile::Empty => 0,
            MmapFile::Mapped(view) => view.len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The whole mapped file as a byte slice, built once from the view
    /// pointer and the length obtained from `GetFileSizeEx`.
    ///
    /// Soundness: see the module-level note. The view is read-only, the
    /// mapping object keeps the file's data addressable for the view's
    /// lifetime, and `self` outlives every slice borrowed from it. The
    /// named residual risk is an *external* writer truncating the file,
    /// which can fault a later read — a documented design trade, not
    /// something this function can silently cause on its own.
    pub fn as_slice(&self) -> &[u8] {
        match self {
            MmapFile::Empty => &[],
            MmapFile::Mapped(view) => {
                // SAFETY: `view.view` is a live read-only mapping of
                // exactly `view.len` bytes (checked non-null at open),
                // owned by this MmapFile, which outlives the returned
                // slice; nothing can mutate the bytes through this
                // mapping.
                unsafe { std::slice::from_raw_parts(view.view as *const u8, view.len as usize) }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "veloedit-mmap-test-{}-{}",
            std::process::id(),
            name
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.bin");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn map_reads_file_bytes() {
        let path = temp_file("basic", b"mapped bytes");
        let mmap = MmapFile::open(&path).unwrap();
        assert_eq!(mmap.len(), 12);
        assert_eq!(mmap.as_slice(), b"mapped bytes");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn zero_length_file_is_the_empty_variant() {
        let path = temp_file("empty", b"");
        let mmap = MmapFile::open(&path).unwrap();
        assert!(matches!(mmap, MmapFile::Empty));
        assert_eq!(mmap.len(), 0);
        assert_eq!(mmap.as_slice(), b"");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn missing_file_fails_at_the_open_step() {
        let path = std::env::temp_dir().join("veloedit-mmap-no-such-file.bin");
        let err = MmapFile::open(&path).unwrap_err();
        assert_eq!(err.step, MmapStep::Open);
    }

    #[test]
    fn mapped_piece_table_reads_through_the_view() {
        let path = temp_file("table", b"hello world");
        let mmap = MmapFile::open(&path).unwrap();
        let mut table = crate::piece_table::PieceTable::from_mapped(std::sync::Arc::new(mmap));
        assert_eq!(table.read(0, 99), b"hello world");
        table.insert(5, b" brave new");
        assert_eq!(table.read(0, 99), b"hello brave new world");
        assert!(table.check_invariants());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
