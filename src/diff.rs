//! Headless binary diff operation (SPEC.md section 7:
//! `VeloEdit.exe --diff A.bin B.bin --output report.json --headless`).
//!
//! The comparison streams both sources in 64 KB chunks, so it runs in
//! constant memory on files of any size. The report counts byte differences
//! exactly over the overlapping range; when the lengths differ, the shorter
//! length is reported as the first difference offset (unless an earlier
//! byte difference exists) and the length gap is reported separately as
//! `extra_bytes`.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

/// Streaming chunk size for reader-based comparison.
const DIFF_CHUNK: usize = 64 * 1024;

/// Result of comparing two byte sources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffReport {
    /// True only when both sources have the same length and every byte
    /// matches.
    pub equal: bool,
    pub len_a: u64,
    pub len_b: u64,
    /// Offset of the first mismatching byte, or of the end of the shorter
    /// source when one is a prefix of the other. `None` when equal.
    pub first_diff_offset: Option<u64>,
    /// Exact number of mismatching byte positions over the overlapping
    /// range `0..min(len_a, len_b)`.
    pub diff_byte_count: u64,
    /// Absolute length difference: `|len_a - len_b|`.
    pub extra_bytes: u64,
}

impl DiffReport {
    /// Render the report as JSON. Hand-rolled on purpose: VeloEdit is
    /// zero-dependency apart from `windows-sys` (SPEC.md section 1.1), so
    /// no serde. The report contains only numbers, a boolean, and a
    /// number-or-null, so no string escaping is involved.
    pub fn to_json(&self) -> String {
        let first = match self.first_diff_offset {
            Some(offset) => offset.to_string(),
            None => "null".to_string(),
        };
        format!(
            "{{\"equal\":{},\"len_a\":{},\"len_b\":{},\"first_diff_offset\":{},\"diff_byte_count\":{},\"extra_bytes\":{}}}",
            self.equal, self.len_a, self.len_b, first, self.diff_byte_count, self.extra_bytes
        )
    }
}

/// Compare two in-memory byte slices.
pub fn diff_bytes(a: &[u8], b: &[u8]) -> DiffReport {
    // Cursor reads never fail, so the io::Result can be unwrapped safely.
    diff_readers(io::Cursor::new(a), io::Cursor::new(b))
        .expect("comparing in-memory slices cannot fail")
}

/// Compare two files by path, streaming both. Files are never loaded whole.
pub fn diff_files(path_a: &Path, path_b: &Path) -> io::Result<DiffReport> {
    let file_a = File::open(path_a)?;
    let file_b = File::open(path_b)?;
    diff_readers(file_a, file_b)
}

/// Compare two readers chunk by chunk in constant memory.
pub fn diff_readers(mut a: impl Read, mut b: impl Read) -> io::Result<DiffReport> {
    let mut buf_a = vec![0u8; DIFF_CHUNK];
    let mut buf_b = vec![0u8; DIFF_CHUNK];
    let mut len_a: u64 = 0;
    let mut len_b: u64 = 0;
    let mut first_diff_offset: Option<u64> = None;
    let mut diff_byte_count: u64 = 0;

    loop {
        let read_a = read_full(&mut a, &mut buf_a)?;
        let read_b = read_full(&mut b, &mut buf_b)?;
        if read_a == 0 && read_b == 0 {
            break;
        }
        // Both readers advance in lockstep by full chunks until one side
        // reaches its end, so the two running totals are equal at every
        // point where `common` can be non-zero.
        let chunk_start = len_a.min(len_b);
        let common = read_a.min(read_b);
        for i in 0..common {
            if buf_a[i] != buf_b[i] {
                if first_diff_offset.is_none() {
                    first_diff_offset = Some(chunk_start + i as u64);
                }
                diff_byte_count += 1;
            }
        }
        if read_a != read_b && first_diff_offset.is_none() {
            // One source ended mid-chunk while the compared bytes matched:
            // the first difference is where the shorter chunk ran out.
            first_diff_offset = Some(chunk_start + common as u64);
        }
        len_a += read_a as u64;
        len_b += read_b as u64;
        if read_a < DIFF_CHUNK && read_b < DIFF_CHUNK {
            break;
        }
    }

    let extra_bytes = len_a.abs_diff(len_b);
    if extra_bytes > 0 && first_diff_offset.is_none() {
        first_diff_offset = Some(len_a.min(len_b));
    }
    Ok(DiffReport {
        equal: diff_byte_count == 0 && extra_bytes == 0,
        len_a,
        len_b,
        first_diff_offset,
        diff_byte_count,
        extra_bytes,
    })
}

/// Fill `buf` as far as the reader allows; returns the total bytes read
/// (short only at end of stream or on a genuinely short read sequence).
fn read_full(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_inputs() {
        let report = diff_bytes(b"same bytes", b"same bytes");
        assert!(report.equal);
        assert_eq!(report.len_a, 10);
        assert_eq!(report.len_b, 10);
        assert_eq!(report.first_diff_offset, None);
        assert_eq!(report.diff_byte_count, 0);
        assert_eq!(report.extra_bytes, 0);
    }

    #[test]
    fn empty_inputs_are_equal() {
        let report = diff_bytes(b"", b"");
        assert!(report.equal);
        assert_eq!(report.len_a, 0);
        assert_eq!(report.first_diff_offset, None);
    }

    #[test]
    fn different_bytes_are_counted_exactly() {
        let report = diff_bytes(b"abcdef", b"abXdeY");
        assert!(!report.equal);
        assert_eq!(report.first_diff_offset, Some(2));
        assert_eq!(report.diff_byte_count, 2);
        assert_eq!(report.extra_bytes, 0);
    }

    #[test]
    fn length_mismatch_with_matching_prefix() {
        let report = diff_bytes(b"abc", b"abcdef");
        assert!(!report.equal);
        assert_eq!(report.len_a, 3);
        assert_eq!(report.len_b, 6);
        assert_eq!(report.first_diff_offset, Some(3));
        assert_eq!(report.diff_byte_count, 0);
        assert_eq!(report.extra_bytes, 3);
    }

    #[test]
    fn length_mismatch_with_earlier_byte_diff() {
        let report = diff_bytes(b"aXcdef", b"abcdefg");
        assert!(!report.equal);
        assert_eq!(report.first_diff_offset, Some(1));
        assert_eq!(report.diff_byte_count, 1);
        assert_eq!(report.extra_bytes, 1);
    }

    #[test]
    fn diff_across_chunk_boundary() {
        // Difference in the second 64 KB chunk must carry the right offset.
        let mut a = vec![7u8; DIFF_CHUNK + 100];
        let mut b = a.clone();
        b[DIFF_CHUNK + 42] = 9;
        let report = diff_bytes(&a, &b);
        assert_eq!(report.first_diff_offset, Some((DIFF_CHUNK + 42) as u64));
        assert_eq!(report.diff_byte_count, 1);
        a[0] = 1;
        let report = diff_bytes(&a, &b);
        assert_eq!(report.first_diff_offset, Some(0));
        assert_eq!(report.diff_byte_count, 2);
    }

    #[test]
    fn json_shape_is_stable() {
        let report = diff_bytes(b"abc", b"abd");
        assert_eq!(
            report.to_json(),
            "{\"equal\":false,\"len_a\":3,\"len_b\":3,\"first_diff_offset\":2,\"diff_byte_count\":1,\"extra_bytes\":0}"
        );
        let equal = diff_bytes(b"x", b"x");
        assert_eq!(
            equal.to_json(),
            "{\"equal\":true,\"len_a\":1,\"len_b\":1,\"first_diff_offset\":null,\"diff_byte_count\":0,\"extra_bytes\":0}"
        );
    }

    #[test]
    fn file_diff_matches_slice_diff() {
        let dir = std::env::temp_dir().join(format!("veloedit-diff-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path_a = dir.join("a.bin");
        let path_b = dir.join("b.bin");
        std::fs::write(&path_a, b"file contents A").unwrap();
        std::fs::write(&path_b, b"file contents B").unwrap();
        let report = diff_files(&path_a, &path_b).unwrap();
        assert_eq!(report, diff_bytes(b"file contents A", b"file contents B"));
        assert!(!report.equal);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
