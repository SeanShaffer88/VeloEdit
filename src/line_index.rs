//! Sparse line offset index (SPEC.md section 3.1).
//!
//! Scanning a 100+ GB file for every line lookup is not possible, so this
//! index stores one checkpoint every 64 KB: the byte offset and how many
//! complete lines end before it. A line lookup binary-searches the
//! checkpoints in O(log M), seeks to the nearest preceding checkpoint, and
//! scans forward only the remaining lines — never the whole file.
//!
//! The scan is word-parallel (roadmap item 3): newline counting
//! processes eight bytes per step with SWAR bit tricks (see
//! [`count_newlines_in`]) — portable, safe code that gets most of the
//! practical win of a SIMD kernel without `unsafe` or CPU-specific
//! intrinsics. The initial build is also parallel: blocks are read in
//! bounded rounds and their newline counts are computed on up to eight
//! worker threads, then assembled in order, so the checkpoints are
//! identical to a purely serial build for every input.

/// Distance between line checkpoints: 64 KB (SPEC.md section 3.1).
pub const CHECKPOINT_INTERVAL: u64 = 64 * 1024;

/// Buffer size for the forward scan in
/// [`LineIndex::line_start_offset`] (kept separate from
/// `CHECKPOINT_INTERVAL`: it bounds one resolver step, not the index
/// density).
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

/// High bits of the bytes in `word` that equal `byte`, via SWAR:
/// broadcast the byte, XOR, then flag every zero byte. The zero test
/// per byte is `(x & 0x7F..) + 0x7F..) | x`: a byte's high bit ends up
/// set iff the byte was non-zero, and no carry can cross a byte
/// boundary (both addends have their high bit clear), so the result
/// is exact — unlike the subtract-based haszero trick, which can
/// over-count through borrow chains.
pub(crate) fn swar_match_high_bits(word: u64, byte: u8) -> u64 {
    let broadcast = u64::from_le_bytes([byte; 8]);
    let x = word ^ broadcast;
    let nonzero = ((x & 0x7F7F_7F7F_7F7F_7F7F) + 0x7F7F_7F7F_7F7F_7F7F) | x;
    !nonzero & 0x8080_8080_8080_8080
}

/// Count occurrences of `byte` in `data`, eight bytes per step with
/// the SWAR matcher above and a scalar tail for the remainder.
pub fn count_byte_swar(data: &[u8], byte: u8) -> u64 {
    let mut total: u64 = 0;
    let mut words = data.chunks_exact(8);
    for w in &mut words {
        let word = u64::from_le_bytes(w.try_into().expect("chunk of 8"));
        total += swar_match_high_bits(word, byte).count_ones() as u64;
    }
    total += words.remainder().iter().filter(|&&b| b == byte).count() as u64;
    total
}

/// Count newline bytes in `data` (word-parallel SWAR; see
/// [`count_byte_swar`]).
pub fn count_newlines_in(data: &[u8]) -> u64 {
    count_byte_swar(data, b'\n')
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
        // Worker count for the parallel counting: the machine's
        // parallelism, capped at 8 (roadmap item 3).
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, 8);

        let mut checkpoints: Vec<LineCheckpoint> = Vec::new();
        let mut lines_before: u64 = 0;
        let mut prev_byte: Option<u8> = None;
        let mut offset: u64 = 0;
        let mut stopped = false;

        // Blocks are read serially (the `read_at` contract is a
        // single-threaded cursor) in rounds of up to `workers` blocks
        // — at most 8 × 64 KB of live memory — and each round's
        // newline counts are computed in parallel, then assembled in
        // order. The checkpoint stream is exactly the serial one.
        while offset < total_len && !stopped {
            let mut blocks: Vec<(u64, Vec<u8>)> = Vec::with_capacity(workers);
            let mut round_offset = offset;
            while blocks.len() < workers && round_offset < total_len {
                let want = (CHECKPOINT_INTERVAL).min(total_len - round_offset) as usize;
                let mut buffer = vec![0u8; want];
                let read = read_at(round_offset, &mut buffer);
                buffer.truncate(read);
                blocks.push((round_offset, buffer));
                if read == 0 {
                    break; // Source ended earlier than promised.
                }
                round_offset += read as u64;
            }
            // Per-block results: (newline count, last byte).
            let results: Vec<(u64, Option<u8>)> = if blocks.len() > 1 {
                std::thread::scope(|scope| {
                    let handles: Vec<_> = blocks
                        .iter()
                        .map(|(_, data)| {
                            scope.spawn(|| (count_newlines_in(data), data.last().copied()))
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| h.join().expect("line-index worker panicked"))
                        .collect()
                })
            } else {
                blocks
                    .iter()
                    .map(|(_, data)| (count_newlines_in(data), data.last().copied()))
                    .collect()
            };
            for ((block_offset, data), (count, last)) in blocks.iter().zip(results.iter()) {
                checkpoints.push(LineCheckpoint {
                    byte_offset: *block_offset,
                    line_number: lines_before,
                    at_line_start: *block_offset == 0 || prev_byte == Some(b'\n'),
                });
                if data.is_empty() {
                    stopped = true; // the read==0 block: index what we saw
                    break;
                }
                lines_before += count;
                prev_byte = *last;
                offset = block_offset + data.len() as u64;
            }
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

    /// The serial reference the parallel build must reproduce exactly.
    fn serial_checkpoints(data: &[u8]) -> Vec<LineCheckpoint> {
        let mut out = Vec::new();
        let mut lines_before = 0u64;
        let mut prev: Option<u8> = None;
        let mut offset = 0u64;
        while offset < data.len() as u64 {
            out.push(LineCheckpoint {
                byte_offset: offset,
                line_number: lines_before,
                at_line_start: offset == 0 || prev == Some(b'\n'),
            });
            let want = (CHECKPOINT_INTERVAL).min(data.len() as u64 - offset) as usize;
            let block = &data[offset as usize..offset as usize + want];
            lines_before += block.iter().filter(|&&b| b == b'\n').count() as u64;
            prev = block.last().copied();
            offset += want as u64;
        }
        if out.is_empty() {
            out.push(LineCheckpoint {
                byte_offset: 0,
                line_number: 0,
                at_line_start: true,
            });
        }
        out
    }

    #[test]
    fn swar_count_matches_scalar_on_adversarial_and_random_data() {
        // Adversarial for borrow-based SWAR: 0x0A beside 0x0B / 0x00 /
        // 0xFF patterns that smear borrows in the naive formula.
        let cases: Vec<&[u8]> = vec![
            b"\n\x0B\n\x00\n\xFF\n",
            b"\x0A\x0A\x0A\x0A\x0A\x0A\x0A\x0A\x0A",
            b"aaaaaaaa\nbbbbbbbb\n",
            b"\xFF\xFE\n\x00\x01\n",
        ];
        for case in cases {
            let naive = case.iter().filter(|&&b| b == b'\n').count() as u64;
            assert_eq!(count_newlines_in(case), naive, "{case:?}");
        }
        // Randomised buffers, all lengths around word boundaries.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for len in [
            0usize, 1, 7, 8, 9, 15, 16, 17, 63, 64, 65, 1000, 8191, 8192, 8193,
        ] {
            let data: Vec<u8> = (0..len)
                .map(|_| {
                    let r = next();
                    if r % 7 == 0 {
                        b'\n'
                    } else {
                        (r % 256) as u8
                    }
                })
                .collect();
            let naive = data.iter().filter(|&&b| b == b'\n').count() as u64;
            assert_eq!(count_newlines_in(&data), naive, "len {len}");
        }
    }

    #[test]
    fn parallel_build_matches_serial_on_randomised_buffers() {
        // Roadmap item 3 equivalence: the parallel build's checkpoints
        // are identical to the serial reference for every input shape:
        // block-straddling newlines, \r\n data, no trailing newline,
        // empty input, exact interval multiples.
        let mut state = 0xDEAD_BEEF_CAFE_F00Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut buffers: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"no newline at all".to_vec(),
            b"trailing newline\n".to_vec(),
            b"crlf\r\nlines\r\nend".to_vec(),
            vec![b'x'; CHECKPOINT_INTERVAL as usize], // exactly one block
            vec![b'\n'; CHECKPOINT_INTERVAL as usize + 1], // straddle
        ];
        // A long random buffer with newlines and CRLF pairs sprinkled
        // across many block boundaries.
        let mut big = Vec::new();
        while big.len() < (CHECKPOINT_INTERVAL as usize) * 9 + 123 {
            match next() % 11 {
                0 => big.extend_from_slice(b"\r\n"),
                1 => big.push(b'\n'),
                _ => big.push(b'a' + (next() % 26) as u8),
            }
        }
        buffers.push(big);
        for data in &buffers {
            let index = LineIndex::build_from_bytes(data);
            assert_eq!(
                index.checkpoints(),
                serial_checkpoints(data).as_slice(),
                "checkpoints for len {}",
                data.len()
            );
            assert_eq!(
                index.total_newlines(),
                data.iter().filter(|&&b| b == b'\n').count() as u64
            );
        }
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
