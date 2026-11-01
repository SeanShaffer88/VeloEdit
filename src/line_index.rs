//! Sparse line offset index (SPEC.md section 3.1).
//!
//! Scanning a 100+ GB file for every line lookup is not possible, so this
//! index stores one checkpoint every 64 KB: the byte offset and how many
//! complete lines end before it. A line lookup binary-searches the
//! checkpoints in O(log M), seeks to the nearest preceding checkpoint, and
//! scans forward only the remaining lines — never the whole file.
//!
//! SPEC.md calls for a SIMD-accelerated scan (AVX2 / SSE4.2) on a secondary
//! thread. This foundation implementation uses a plain chunked scan with no
//! `unsafe`; the newline counting is isolated in [`count_newlines_in`] and
//! the per-chunk inner loop is structured so a SIMD implementation can
//! replace it later without changing this module's API. Whether the scan
//! runs on a background thread is a coordinator decision for a later slice;
//! building the index itself is synchronous and bounded here.

/// Distance between line checkpoints: 64 KB (SPEC.md section 3.1).
pub const CHECKPOINT_INTERVAL: u64 = 64 * 1024;

/// Size of the sub-chunks the newline counter walks. Kept separate from
/// `CHECKPOINT_INTERVAL` so a SIMD inner loop can adopt its own width.
const COUNT_CHUNK: usize = 8192;

/// One sparse checkpoint: a known line number at a known byte offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineCheckpoint {
    /// Byte offset of the checkpoint (a multiple of `CHECKPOINT_INTERVAL`,
    /// except possibly a trailing partial checkpoint in tests).
    pub byte_offset: u64,
    /// Number of newline bytes in `[0, byte_offset)` — i.e. the 0-based
    /// index of the line that contains `byte_offset`.
    pub line_number: u64,
    /// True when `byte_offset` is exactly the start of a line (offset 0, or
    /// the preceding byte is `'\n'`).
    pub at_line_start: bool,
}

/// Count newline bytes in `data`.
///
/// This is the single seam where a SIMD implementation (SPEC.md section 3.1)
/// replaces the inner loop: it walks the input in fixed sub-chunks and
/// counts within each chunk, exactly the shape a vectorised
/// compare-and-popcount kernel consumes.
pub fn count_newlines_in(data: &[u8]) -> u64 {
    let mut total: u64 = 0;
    for chunk in data.chunks(COUNT_CHUNK) {
        total += chunk.iter().filter(|&&b| b == b'\n').count() as u64;
    }
    total
}

/// The sparse checkpoint index over one byte source.
#[derive(Debug)]
pub struct LineIndex {
    checkpoints: Vec<LineCheckpoint>,
    total_length: u64,
    total_newlines: u64,
    ends_with_newline: bool,
}

impl LineIndex {
    /// Build the index over a byte source of `total_len` bytes. `read_at`
    /// fills the provided buffer with bytes starting at the given offset and
    /// returns how many bytes it wrote; it must return 0 only at the end of
    /// the source. The scan proceeds one checkpoint interval at a time, so
    /// the live memory cost is one 64 KB buffer regardless of file size.
    pub fn build(total_len: u64, mut read_at: impl FnMut(u64, &mut [u8]) -> usize) -> Self {
        let mut checkpoints: Vec<LineCheckpoint> = Vec::new();
        let mut lines_before: u64 = 0;
        let mut prev_byte: Option<u8> = None;
        let mut buffer = vec![0u8; CHECKPOINT_INTERVAL as usize];
        let mut offset: u64 = 0;

        while offset < total_len {
            checkpoints.push(LineCheckpoint {
                byte_offset: offset,
                line_number: lines_before,
                at_line_start: offset == 0 || prev_byte == Some(b'\n'),
            });
            let want = (CHECKPOINT_INTERVAL).min(total_len - offset) as usize;
            let read = read_at(offset, &mut buffer[..want]);
            if read == 0 {
                break; // Source ended earlier than promised; index what we saw.
            }
            let data = &buffer[..read];
            lines_before += count_newlines_in(data);
            prev_byte = data.last().copied();
            offset += read as u64;
        }
        if checkpoints.is_empty() {
            checkpoints.push(LineCheckpoint {
                byte_offset: 0,
                line_number: 0,
                at_line_start: true,
            });
        }

        Self {
            checkpoints,
            total_length: total_len,
            total_newlines: lines_before,
            ends_with_newline: prev_byte == Some(b'\n'),
        }
    }

    /// Build the index over an in-memory byte slice (tests, small files).
    pub fn build_from_bytes(data: &[u8]) -> Self {
        Self::build(data.len() as u64, |offset, out| {
            let start = offset as usize;
            if start >= data.len() {
                return 0;
            }
            let n = out.len().min(data.len() - start);
            out[..n].copy_from_slice(&data[start..start + n]);
            n
        })
    }

    pub fn checkpoints(&self) -> &[LineCheckpoint] {
        &self.checkpoints
    }

    pub fn total_length(&self) -> u64 {
        self.total_length
    }

    pub fn total_newlines(&self) -> u64 {
        self.total_newlines
    }

    /// Number of lines: every newline-terminated line, plus one final
    /// partial line when the source does not end with a newline. An empty
    /// source has zero lines.
    pub fn line_count(&self) -> u64 {
        if self.total_length == 0 {
            0
        } else if self.ends_with_newline {
            self.total_newlines
        } else {
            self.total_newlines + 1
        }
    }

    /// The nearest checkpoint at or before `byte_offset`, found by binary
    /// search in O(log M) (SPEC.md section 3.1).
    pub fn nearest_checkpoint(&self, byte_offset: u64) -> LineCheckpoint {
        let idx = self
            .checkpoints
            .partition_point(|cp| cp.byte_offset <= byte_offset);
        if idx == 0 {
            self.checkpoints[0]
        } else {
            self.checkpoints[idx - 1]
        }
    }

    /// Byte offset where 0-based line `line_no` starts.
    ///
    /// Picks the best checkpoint — the last one that either sits exactly on
    /// the start of `line_no` or counts fewer lines — then scans forward
    /// with `read_at` (same contract as [`LineIndex::build`]) across only
    /// the remaining lines. Returns `None` when `line_no` is past the last
    /// line of the source.
    pub fn line_start_offset(
        &self,
        line_no: u64,
        mut read_at: impl FnMut(u64, &mut [u8]) -> usize,
    ) -> Option<u64> {
        if line_no == 0 {
            return Some(0);
        }
        if line_no >= self.line_count() {
            return None;
        }
        // The predicate is monotonic over the checkpoint list: checkpoints
        // are ordered by line_number, and within one line_number the
        // at-line-start checkpoints come first.
        let idx = self.checkpoints.partition_point(|cp| {
            cp.line_number < line_no || (cp.line_number == line_no && cp.at_line_start)
        });
        if idx == 0 {
            return None;
        }
        let cp = self.checkpoints[idx - 1];
        if cp.line_number == line_no && cp.at_line_start {
            return Some(cp.byte_offset);
        }
        let mut remaining = line_no - cp.line_number;
        let mut pos = cp.byte_offset;
        let mut buffer = [0u8; COUNT_CHUNK];
        while pos < self.total_length {
            let want = (COUNT_CHUNK as u64).min(self.total_length - pos) as usize;
            let read = read_at(pos, &mut buffer[..want]);
            if read == 0 {
                return None;
            }
            for (i, &b) in buffer[..read].iter().enumerate() {
                if b == b'\n' {
                    remaining -= 1;
                    if remaining == 0 {
                        return Some(pos + i as u64 + 1);
                    }
                }
            }
            pos += read as u64;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader_for(data: &[u8]) -> impl FnMut(u64, &mut [u8]) -> usize + '_ {
        move |offset: u64, out: &mut [u8]| {
            let start = offset as usize;
            if start >= data.len() {
                return 0;
            }
            let n = out.len().min(data.len() - start);
            out[..n].copy_from_slice(&data[start..start + n]);
            n
        }
    }

    #[test]
    fn count_newlines_basic() {
        assert_eq!(count_newlines_in(b""), 0);
        assert_eq!(count_newlines_in(b"no breaks"), 0);
        assert_eq!(count_newlines_in(b"a\nb\nc\n"), 3);
        assert_eq!(count_newlines_in(b"\n\n\n"), 3);
    }

    #[test]
    fn small_text_index() {
        let data = b"one\ntwo\nthree\nfour";
        let index = LineIndex::build_from_bytes(data);
        assert_eq!(index.checkpoints().len(), 1); // fits in one interval
        assert_eq!(index.checkpoints()[0].byte_offset, 0);
        assert_eq!(index.checkpoints()[0].line_number, 0);
        assert_eq!(index.total_newlines(), 3);
        assert_eq!(index.line_count(), 4); // 3 terminated + final partial "four"

        assert_eq!(index.line_start_offset(0, reader_for(data)), Some(0));
        assert_eq!(index.line_start_offset(1, reader_for(data)), Some(4));
        assert_eq!(index.line_start_offset(2, reader_for(data)), Some(8));
        assert_eq!(index.line_start_offset(3, reader_for(data)), Some(14));
        assert_eq!(index.line_start_offset(4, reader_for(data)), None);
    }

    #[test]
    fn trailing_newline_means_no_partial_line() {
        let data = b"a\nb\n";
        let index = LineIndex::build_from_bytes(data);
        assert_eq!(index.line_count(), 2);
        assert_eq!(index.line_start_offset(1, reader_for(data)), Some(2));
        assert_eq!(index.line_start_offset(2, reader_for(data)), None);
    }

    #[test]
    fn empty_source_has_no_lines() {
        let index = LineIndex::build_from_bytes(b"");
        assert_eq!(index.line_count(), 0);
        assert_eq!(index.line_start_offset(0, reader_for(b"")), Some(0));
        assert_eq!(index.line_start_offset(1, reader_for(b"")), None);
    }

    /// Build a document of known lines spanning several checkpoint
    /// intervals: each line is "line NNNNNN" + padding + newline, 32 bytes.
    fn synthetic_doc(lines: u64) -> Vec<u8> {
        let mut data = Vec::new();
        for i in 0..lines {
            let mut line = format!("line {i:06}").into_bytes();
            line.resize(31, b'.');
            line.push(b'\n');
            debug_assert_eq!(line.len(), 32);
            data.extend_from_slice(&line);
        }
        data
    }

    #[test]
    fn checkpoints_every_64kb_with_correct_line_numbers() {
        // 6_000 lines * 32 bytes = 192_000 bytes: checkpoints at 0, 65536,
        // 131072. Each interval holds exactly 2048 lines.
        let data = synthetic_doc(6_000);
        let index = LineIndex::build_from_bytes(&data);
        let cps = index.checkpoints();
        assert_eq!(cps.len(), 3);
        assert_eq!(cps[0].byte_offset, 0);
        assert_eq!(cps[0].line_number, 0);
        assert_eq!(cps[1].byte_offset, CHECKPOINT_INTERVAL);
        assert_eq!(cps[1].line_number, 2048);
        assert!(cps[1].at_line_start); // 65536 is a multiple of line size 32
        assert_eq!(cps[2].byte_offset, 2 * CHECKPOINT_INTERVAL);
        assert_eq!(cps[2].line_number, 4096);
        assert_eq!(index.line_count(), 6_000);
    }

    #[test]
    fn nearest_checkpoint_binary_search() {
        let data = synthetic_doc(6_000);
        let index = LineIndex::build_from_bytes(&data);
        assert_eq!(index.nearest_checkpoint(0).byte_offset, 0);
        assert_eq!(index.nearest_checkpoint(65_535).byte_offset, 0);
        assert_eq!(
            index.nearest_checkpoint(65_536).byte_offset,
            CHECKPOINT_INTERVAL
        );
        assert_eq!(
            index.nearest_checkpoint(70_000).byte_offset,
            CHECKPOINT_INTERVAL
        );
        assert_eq!(
            index.nearest_checkpoint(u64::MAX).byte_offset,
            2 * CHECKPOINT_INTERVAL
        );
    }

    #[test]
    fn line_start_across_checkpoints() {
        let data = synthetic_doc(6_000);
        let index = LineIndex::build_from_bytes(&data);
        // Line starts are at line_no * 32 by construction.
        for line in [1u64, 100, 2047, 2048, 2049, 4096, 5000, 5999] {
            assert_eq!(
                index.line_start_offset(line, reader_for(&data)),
                Some(line * 32),
                "line {line}"
            );
        }
        assert_eq!(index.line_start_offset(6000, reader_for(&data)), None);
    }

    #[test]
    fn line_start_when_checkpoint_lands_mid_line() {
        // 100-byte lines: checkpoint at 65536 lands mid-line (65536 % 100
        // == 36), so the resolver must start from an earlier-count
        // checkpoint and scan forward.
        let mut data = Vec::new();
        for _ in 0..1000 {
            let mut line = vec![b'x'; 99];
            line.push(b'\n');
            data.extend_from_slice(&line);
        }
        let index = LineIndex::build_from_bytes(&data);
        let cp = index.nearest_checkpoint(CHECKPOINT_INTERVAL);
        assert_eq!(cp.byte_offset, CHECKPOINT_INTERVAL);
        assert!(!cp.at_line_start);
        assert_eq!(cp.line_number, 655); // 655 complete lines in 65536 bytes
        for line in [656u64, 700, 999] {
            assert_eq!(
                index.line_start_offset(line, reader_for(&data)),
                Some(line * 100),
                "line {line}"
            );
        }
    }
}
