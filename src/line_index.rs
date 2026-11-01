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

/// Bytes of index built synchronously when a document is opened
/// (the staged-open head region). The first screens need only a few
/// hundred lines; 4 MB of checkpoints covers tens of thousands of
/// typical lines, so the window can paint and take input while the
/// background build finishes the rest. A document no larger than
/// this is indexed whole by the head build — its behaviour is
/// exactly the old synchronous one.
pub const INDEX_HEAD_BYTES: u64 = 4 * 1024 * 1024;

/// Completion percentage of a staged index build, for the status
/// readout: whole percent of `total` covered by `indexed`, capped
/// at 100; a zero-length source is trivially complete.
pub fn indexing_percent(indexed: u64, total: u64) -> u32 {
    if total == 0 {
        return 100;
    }
    ((indexed.min(total) as u128 * 100) / total as u128) as u32
}

/// The status-bar suffix for an in-progress staged build —
/// `" · Indexing NN%"` — or `None` when there is nothing to report
/// (no document, or the build has covered the whole source).
pub fn indexing_status_suffix(indexed: u64, total: u64) -> Option<String> {
    if total == 0 || indexed >= total {
        None
    } else {
        Some(format!(
            " \u{00B7} Indexing {}%",
            indexing_percent(indexed, total)
        ))
    }
}

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

// Layout pin (architectural audit): the index holds one checkpoint per
// `CHECKPOINT_INTERVAL` (64 KB) of source, so a complete index costs
// exactly `source_len / 64 KB * 24` bytes — the figure the SPEC
// section 8 memory budget implicitly spends on indexing. If a field
// change grows this record, the build breaks here instead of silently
// inflating the index for every large file.
const _: () = assert!(std::mem::size_of::<LineCheckpoint>() == 24);
const _: () = assert!(std::mem::align_of::<LineCheckpoint>() == 8);

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

/// A resumable line-index build: the scan behind
/// [`LineIndex::build`], exposed in stages so a huge source can be
/// indexed a piece at a time — a head region synchronously at open,
/// the rest on a background thread — with snapshots taken along the
/// way. Every stage uses the same round machinery, so a staged
/// build's checkpoints are identical to a one-shot build's, prefix
/// for prefix (pinned by tests).
#[derive(Debug)]
pub struct LineIndexBuilder {
    total_len: u64,
    checkpoints: Vec<LineCheckpoint>,
    lines_before: u64,
    prev_byte: Option<u8>,
    /// Frontier: bytes fully indexed so far.
    offset: u64,
    /// The source ended earlier than promised (a read returned 0).
    stopped: bool,
    /// Scratch reused across scan rounds (architectural audit):
    /// `advance` works in rounds of up to eight 64 KB blocks, and used
    /// to allocate the block buffers, the block list and the result
    /// list fresh for every round — for a whole-file build that is
    /// three allocations per 512 KB scanned, plus one per staged
    /// `advance` call. The pool lives on the builder, so the same
    /// buffers serve every round of every stage; at most eight 64 KB
    /// buffers are ever held, the bound the module docs promise.
    scratch_buffers: Vec<Vec<u8>>,
    scratch_blocks: Vec<(u64, Vec<u8>)>,
    scratch_results: Vec<(u64, Option<u8>)>,
}

impl LineIndexBuilder {
    pub fn new(total_len: u64) -> Self {
        Self {
            total_len,
            checkpoints: Vec::new(),
            lines_before: 0,
            prev_byte: None,
            offset: 0,
            stopped: false,
            scratch_buffers: Vec::new(),
            scratch_blocks: Vec::new(),
            scratch_results: Vec::new(),
        }
    }

    /// The promised length of the source being indexed.
    pub fn total_len(&self) -> u64 {
        self.total_len
    }

    /// Bytes indexed so far (the frontier).
    pub fn indexed_bytes(&self) -> u64 {
        self.offset
    }

    /// True when the whole source has been indexed — the frontier
    /// reached the promised length, or the source ended early (in
    /// which case everything that exists is indexed).
    pub fn is_complete(&self) -> bool {
        self.stopped || self.offset >= self.total_len
    }

    /// Index forward until the frontier reaches `limit` (or the
    /// source is complete). Work proceeds in the same rounds as the
    /// one-shot build, so the frontier may overshoot `limit` by up
    /// to one round (at most 8 × 64 KB). `read_at` follows the
    /// contract of [`LineIndex::build`]. Returns [`Self::is_complete`].
    pub fn advance(
        &mut self,
        limit: u64,
        mut read_at: impl FnMut(u64, &mut [u8]) -> usize,
    ) -> bool {
        // Worker count for the parallel counting: the machine's
        // parallelism, capped at 8 (roadmap item 3).
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, 8);

        // A builder resumed from a snapshot of a grown source (see
        // `resume_from`) can start mid-interval: its frontier is the
        // old source's end, not a checkpoint multiple. Top the
        // partial interval up first — count its newlines WITHOUT
        // pushing a checkpoint — so the checkpoint grid stays on
        // CHECKPOINT_INTERVAL multiples and the finished index is
        // identical to a one-shot build. (A fresh or staged builder
        // always sits on a multiple, so this never runs for them.)
        // The top-up may overshoot `limit` by less than one
        // interval, inside the round-overshoot allowance below.
        while !self.stopped && self.offset < self.total_len && self.offset < limit {
            let misalignment = self.offset % CHECKPOINT_INTERVAL;
            if misalignment == 0 {
                break;
            }
            let grid_end = self.offset + (CHECKPOINT_INTERVAL - misalignment);
            let want = (grid_end.min(self.total_len) - self.offset) as usize;
            let mut buffer = self.scratch_buffers.pop().unwrap_or_default();
            buffer.clear();
            buffer.resize(want, 0);
            let read = read_at(self.offset, &mut buffer);
            buffer.truncate(read);
            if read == 0 {
                self.stopped = true;
            } else {
                self.lines_before += count_newlines_in(&buffer);
                self.prev_byte = buffer.last().copied();
                self.offset += read as u64;
            }
            self.scratch_buffers.push(buffer);
        }

        // Blocks are read serially (the `read_at` contract is a
        // single-threaded cursor) in rounds of up to `workers` blocks
        // — at most 8 × 64 KB of live memory — and each round's
        // newline counts are computed in parallel, then assembled in
        // order. The checkpoint stream is exactly the serial one.
        while self.offset < self.total_len && !self.stopped && self.offset < limit {
            // Take the round's working lists out of the scratch pool;
            // they go back at the end of the round (see below), so a
            // build allocates them once, not once per round.
            let mut blocks: Vec<(u64, Vec<u8>)> = std::mem::take(&mut self.scratch_blocks);
            let mut round_offset = self.offset;
            while blocks.len() < workers && round_offset < self.total_len {
                let want = (CHECKPOINT_INTERVAL).min(self.total_len - round_offset) as usize;
                let mut buffer = self.scratch_buffers.pop().unwrap_or_default();
                buffer.clear();
                buffer.resize(want, 0);
                let read = read_at(round_offset, &mut buffer);
                buffer.truncate(read);
                blocks.push((round_offset, buffer));
                if read == 0 {
                    break; // Source ended earlier than promised.
                }
                round_offset += read as u64;
            }
            // Per-block results: (newline count, last byte).
            let mut results: Vec<(u64, Option<u8>)> = std::mem::take(&mut self.scratch_results);
            results.clear();
            if blocks.len() > 1 {
                std::thread::scope(|scope| {
                    let handles: Vec<_> = blocks
                        .iter()
                        .map(|(_, data)| {
                            scope.spawn(|| (count_newlines_in(data), data.last().copied()))
                        })
                        .collect();
                    results.extend(
                        handles
                            .into_iter()
                            .map(|h| h.join().expect("line-index worker panicked")),
                    );
                });
            } else {
                results.extend(
                    blocks
                        .iter()
                        .map(|(_, data)| (count_newlines_in(data), data.last().copied())),
                );
            }
            for ((block_offset, data), (count, last)) in blocks.iter().zip(results.iter()) {
                self.checkpoints.push(LineCheckpoint {
                    byte_offset: *block_offset,
                    line_number: self.lines_before,
                    at_line_start: *block_offset == 0 || self.prev_byte == Some(b'\n'),
                });
                if data.is_empty() {
                    self.stopped = true; // the read==0 block: index what we saw
                    break;
                }
                self.lines_before += count;
                self.prev_byte = *last;
                self.offset = block_offset + data.len() as u64;
            }
            // Recycle the round's buffers and lists into the pool.
            for (_, buffer) in blocks.drain(..) {
                self.scratch_buffers.push(buffer);
            }
            self.scratch_blocks = blocks;
            results.clear();
            self.scratch_results = results;
        }
        self.is_complete()
    }

    /// The index as it stands: every checkpoint so far, marked
    /// partial unless the build is complete. For a partial snapshot
    /// the totals describe the indexed prefix only.
    pub fn snapshot(&self) -> LineIndex {
        let mut checkpoints = self.checkpoints.clone();
        if checkpoints.is_empty() {
            checkpoints.push(LineCheckpoint {
                byte_offset: 0,
                line_number: 0,
                at_line_start: true,
            });
        }
        LineIndex {
            checkpoints,
            total_length: self.total_len,
            total_newlines: self.lines_before,
            ends_with_newline: self.prev_byte == Some(b'\n'),
            indexed_bytes: self.offset,
            complete: self.is_complete(),
        }
    }

    /// Consume the builder into its final snapshot. Callers that
    /// need a complete index drive [`Self::advance`] to completion
    /// first; finishing early yields the partial snapshot.
    pub fn finish(self) -> LineIndex {
        self.snapshot()
    }

    /// Resume a build from a snapshot taken over the same source
    /// (roadmap item 22, Ghost Tail): the snapshot's checkpoints,
    /// newline total and frontier seed the new builder, so
    /// [`Self::advance`] continues exactly where the snapshotted
    /// build stopped and the finished index is checkpoint-identical
    /// to a one-shot build — the growth path only ever has to scan
    /// the appended tail. When the snapshot's source has since
    /// grown, pass the *extended* snapshot (see
    /// [`LineIndex::extended_to`]): the builder's promised length is
    /// the snapshot's `total_length`, so the extension is what
    /// tells it about the new bytes. A snapshot with no bytes
    /// indexed resumes as a fresh build (its checkpoint list may
    /// hold only the synthesised origin checkpoint, which a fresh
    /// builder reproduces itself).
    pub fn resume_from(index: &LineIndex) -> LineIndexBuilder {
        if index.indexed_bytes() == 0 {
            return LineIndexBuilder::new(index.total_length());
        }
        // A snapshot taken after a build stopped early (the source
        // ended before its promised length) can carry a checkpoint
        // exactly at the frontier — the empty block that discovered
        // the end. Drop any such trailing checkpoints: the resumed
        // build pushes the frontier's checkpoint itself when it
        // gets there, and the newline accounting comes from the
        // snapshot's total, not from the dropped entries.
        let mut checkpoints = index.checkpoints().to_vec();
        while checkpoints
            .last()
            .is_some_and(|cp| cp.byte_offset >= index.indexed_bytes())
        {
            checkpoints.pop();
        }
        LineIndexBuilder {
            total_len: index.total_length(),
            checkpoints,
            lines_before: index.total_newlines(),
            // The byte before the frontier matters only through
            // "was it a newline" (the next checkpoint's
            // at_line_start), which the snapshot's
            // ends_with_newline answers exactly.
            prev_byte: Some(if index.ends_with_newline { b'\n' } else { 0 }),
            offset: index.indexed_bytes(),
            stopped: false,
            scratch_buffers: Vec::new(),
            scratch_blocks: Vec::new(),
            scratch_results: Vec::new(),
        }
    }
}

/// The sparse checkpoint index over one byte source.
///
/// An index is either *complete* (it describes the whole source) or
/// *partial* — a staged build in progress, whose checkpoints cover
/// only the prefix `[0, indexed_bytes)`. For a partial index,
/// [`LineIndex::total_newlines`] and [`LineIndex::line_count`]
/// describe that prefix, never the whole source; consumers that
/// present totals must check [`LineIndex::is_complete`] first.
#[derive(Debug, Clone)]
pub struct LineIndex {
    checkpoints: Vec<LineCheckpoint>,
    total_length: u64,
    total_newlines: u64,
    ends_with_newline: bool,
    indexed_bytes: u64,
    complete: bool,
}

impl LineIndex {
    /// Build the index over a byte source of `total_len` bytes. `read_at`
    /// fills the provided buffer with bytes starting at the given offset and
    /// returns how many bytes it wrote; it must return 0 only at the end of
    /// the source. The scan proceeds one checkpoint interval at a time, so
    /// the live memory cost is the builder's reused pool of at most eight
    /// 64 KB block buffers, regardless of file size.
    pub fn build(total_len: u64, read_at: impl FnMut(u64, &mut [u8]) -> usize) -> Self {
        let mut builder = LineIndexBuilder::new(total_len);
        builder.advance(u64::MAX, read_at);
        builder.finish()
    }

    /// Build only the index of the prefix `[0, limit)` of a source
    /// (up to one build round past `limit`): the staged-open head
    /// build. The result is complete when the source is no larger
    /// than the head region, partial otherwise; a
    /// [`LineIndexBuilder`] can later continue the same scan to
    /// completion with identical checkpoints.
    pub fn build_prefix(
        total_len: u64,
        limit: u64,
        read_at: impl FnMut(u64, &mut [u8]) -> usize,
    ) -> Self {
        let mut builder = LineIndexBuilder::new(total_len);
        builder.advance(limit, read_at);
        builder.finish()
    }

    /// Bytes covered by this index: the whole source for a complete
    /// index, the staged frontier for a partial one.
    pub fn indexed_bytes(&self) -> u64 {
        self.indexed_bytes
    }

    /// Re-promise this index against a longer source (roadmap item
    /// 22, Ghost Tail): when the source grows by appends, the bytes
    /// already indexed are unchanged, so the checkpoints, newline
    /// total and frontier carry over verbatim and only the promised
    /// length grows — the result is the partial index of the grown
    /// source, ready to be finished by
    /// [`LineIndexBuilder::resume_from`]. Returns `None` when
    /// `new_total_len` is below the length this index already
    /// describes (that is not growth).
    pub fn extended_to(&self, new_total_len: u64) -> Option<LineIndex> {
        if new_total_len < self.total_length {
            return None;
        }
        let mut extended = self.clone();
        extended.total_length = new_total_len;
        extended.complete = self.complete && new_total_len == self.total_length;
        Some(extended)
    }

    /// True when this index describes the whole source. A partial
    /// index's totals are prefix totals (see the type docs).
    pub fn is_complete(&self) -> bool {
        self.complete
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

    /// Number of lines in the region this index describes: every
    /// newline-terminated line, plus one final partial line when
    /// the region does not end with a newline. An empty region has
    /// zero lines. For a complete index the region is the whole
    /// source; for a partial one it is the indexed prefix, so the
    /// count is the prefix's line count — never a claimed total
    /// for the whole source.
    pub fn line_count(&self) -> u64 {
        if self.indexed_bytes == 0 {
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

    /// The best checkpoint to resolve `line_no` from: the last one
    /// that either sits exactly on the start of `line_no` or counts
    /// fewer lines. The predicate is monotonic over the checkpoint
    /// list: checkpoints are ordered by line_number, and within one
    /// line_number the at-line-start checkpoints come first.
    fn start_checkpoint(&self, line_no: u64) -> Option<LineCheckpoint> {
        let idx = self.checkpoints.partition_point(|cp| {
            cp.line_number < line_no || (cp.line_number == line_no && cp.at_line_start)
        });
        if idx == 0 {
            None
        } else {
            Some(self.checkpoints[idx - 1])
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
        let cp = self.start_checkpoint(line_no)?;
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

    /// Byte offset where 0-based line `line_no` starts, resolved
    /// from a *partial* index: the best checkpoint is picked exactly
    /// as in [`LineIndex::line_start_offset`], but there is no
    /// line-count gate — the target line may lie past the indexed
    /// frontier, so the scan walks forward from the last checkpoint,
    /// counting newlines a 64 KB block at a time (the same SWAR
    /// counter the build uses) and scanning byte-wise only inside
    /// the block where the line starts. Returns `None` only when
    /// the scan reaches the end of the source without finding the
    /// line. The cost is the distance from the nearest checkpoint,
    /// paid at scan speed; for lines inside the indexed prefix that
    /// distance is under one checkpoint interval, exactly as with a
    /// complete index.
    pub fn line_start_offset_forward(
        &self,
        line_no: u64,
        mut read_at: impl FnMut(u64, &mut [u8]) -> usize,
    ) -> Option<u64> {
        if line_no == 0 {
            return Some(0);
        }
        let cp = self.start_checkpoint(line_no)?;
        if cp.line_number == line_no && cp.at_line_start {
            return Some(cp.byte_offset);
        }
        let mut remaining = line_no - cp.line_number;
        let mut pos = cp.byte_offset;
        let mut buffer = vec![0u8; CHECKPOINT_INTERVAL as usize];
        while pos < self.total_length {
            let want = (CHECKPOINT_INTERVAL).min(self.total_length - pos) as usize;
            let read = read_at(pos, &mut buffer[..want]);
            if read == 0 {
                return None;
            }
            let data = &buffer[..read];
            let in_block = count_newlines_in(data);
            if in_block < remaining {
                remaining -= in_block;
                pos += read as u64;
                continue;
            }
            // The line starts inside this block: find the
            // `remaining`-th newline byte-wise.
            let mut seen: u64 = 0;
            for (i, &b) in data.iter().enumerate() {
                if b == b'\n' {
                    seen += 1;
                    if seen == remaining {
                        let start = pos + i as u64 + 1;
                        // A start exactly at the end of the source
                        // is the phantom line after a trailing
                        // newline: the line-count model says it
                        // does not exist (the bounded resolver's
                        // gate rejects it the same way).
                        if start >= self.total_length {
                            return None;
                        }
                        return Some(start);
                    }
                }
            }
            return None; // the block count said the newline is here
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
    fn indexing_percent_and_suffix() {
        assert_eq!(indexing_percent(0, 0), 100);
        assert_eq!(indexing_percent(0, 200), 0);
        assert_eq!(indexing_percent(50, 200), 25);
        assert_eq!(indexing_percent(199, 200), 99);
        assert_eq!(indexing_percent(200, 200), 100);
        assert_eq!(indexing_percent(300, 200), 100); // capped
                                                     // Huge values must not overflow the multiply (and
                                                     // u64::MAX / 2 is just under half of u64::MAX, hence 49).
        assert_eq!(indexing_percent(u64::MAX / 2, u64::MAX), 49);
        assert_eq!(indexing_percent(u64::MAX, u64::MAX), 100);

        assert_eq!(indexing_status_suffix(0, 0), None);
        assert_eq!(indexing_status_suffix(10, 10), None);
        assert_eq!(
            indexing_status_suffix(1, 4).as_deref(),
            Some(" \u{00B7} Indexing 25%")
        );
        assert_eq!(
            indexing_status_suffix(0, 4).as_deref(),
            Some(" \u{00B7} Indexing 0%")
        );
    }

    #[test]
    fn staged_build_prefixes_match_the_one_shot_build() {
        // Staged open, core level: build a head prefix, then keep
        // advancing in steps — every snapshot's checkpoints must be
        // a prefix of the one-shot build's, and the finished staged
        // build must equal it exactly.
        let data = synthetic_doc(200_000); // 6.4 MB, 200 checkpoints
        let full = LineIndex::build_from_bytes(&data);
        assert!(full.is_complete());
        assert_eq!(full.indexed_bytes(), data.len() as u64);

        let mut builder = LineIndexBuilder::new(data.len() as u64);
        let mut reader = reader_for(&data);
        assert!(!builder.advance(INDEX_HEAD_BYTES, &mut reader));
        let head = builder.snapshot();
        assert!(!head.is_complete());
        assert!(head.indexed_bytes() >= INDEX_HEAD_BYTES);
        assert!(head.indexed_bytes() < data.len() as u64);
        // The head snapshot's checkpoints are the full build's prefix.
        assert_eq!(
            head.checkpoints(),
            &full.checkpoints()[..head.checkpoints().len()]
        );
        // Its totals describe the prefix only: the newlines in
        // [0, indexed_bytes) and that region's line count.
        let prefix = &data[..head.indexed_bytes() as usize];
        assert_eq!(
            head.total_newlines(),
            prefix.iter().filter(|&&b| b == b'\n').count() as u64
        );
        assert_eq!(
            head.line_count(),
            LineIndex::build_from_bytes(prefix).line_count()
        );

        // Advance in uneven steps; identity must hold at the end.
        while !builder.advance(builder.indexed_bytes() + 1_000_000, &mut reader) {}
        let staged = builder.finish();
        assert!(staged.is_complete());
        assert_eq!(staged.checkpoints(), full.checkpoints());
        assert_eq!(staged.total_newlines(), full.total_newlines());
        assert_eq!(staged.line_count(), full.line_count());

        // build_prefix is the one-call form of the head stage.
        let head2 = LineIndex::build_prefix(data.len() as u64, INDEX_HEAD_BYTES, reader_for(&data));
        assert_eq!(head2.checkpoints(), head.checkpoints());
        assert_eq!(head2.indexed_bytes(), head.indexed_bytes());
    }

    #[test]
    fn resume_from_a_snapshot_finishes_identically() {
        // Ghost Tail core, index level: index a prefix, snapshot,
        // resume, finish — the result must be checkpoint-identical
        // to the one-shot build of the same source.
        let data = synthetic_doc(200_000); // 6.4 MB
        let full = LineIndex::build_from_bytes(&data);
        let head = LineIndex::build_prefix(data.len() as u64, INDEX_HEAD_BYTES, reader_for(&data));
        let mut builder = LineIndexBuilder::resume_from(&head);
        assert_eq!(builder.indexed_bytes(), head.indexed_bytes());
        assert_eq!(builder.total_len(), data.len() as u64);
        let mut reader = reader_for(&data);
        assert!(builder.advance(u64::MAX, &mut reader));
        let resumed = builder.finish();
        assert_eq!(resumed.checkpoints(), full.checkpoints());
        assert_eq!(resumed.total_newlines(), full.total_newlines());
        assert_eq!(resumed.line_count(), full.line_count());
    }

    #[test]
    fn growth_extension_and_resume_scan_only_the_tail() {
        // The full Ghost Tail index flow: the source is indexed
        // completely at its old length; the file then grows. The
        // extended snapshot keeps every old checkpoint, and the
        // resumed build finishes with checkpoints identical to a
        // one-shot build of the grown source.
        let old = synthetic_doc(3_000); // 96_000 bytes
        let tail = synthetic_doc(2_000);
        let mut grown = old.clone();
        grown.extend_from_slice(&tail);
        let old_index = LineIndex::build_from_bytes(&old);
        assert!(old_index.is_complete());
        let extended = old_index
            .extended_to(grown.len() as u64)
            .expect("growth extends");
        assert!(!extended.is_complete());
        assert_eq!(extended.total_length(), grown.len() as u64);
        assert_eq!(extended.indexed_bytes(), old.len() as u64);
        assert_eq!(extended.checkpoints(), old_index.checkpoints());
        assert_eq!(extended.total_newlines(), old_index.total_newlines());
        // Shrinking the promise is refused, the same length is the
        // identity.
        assert!(old_index.extended_to(old.len() as u64 - 1).is_none());
        assert_eq!(
            old_index
                .extended_to(old.len() as u64)
                .unwrap()
                .checkpoints(),
            old_index.checkpoints()
        );
        let mut builder = LineIndexBuilder::resume_from(&extended);
        let mut reader = reader_for(&grown);
        assert!(builder.advance(u64::MAX, &mut reader));
        let finished = builder.finish();
        let one_shot = LineIndex::build_from_bytes(&grown);
        assert_eq!(finished.checkpoints(), one_shot.checkpoints());
        assert_eq!(finished.line_count(), 5_000);
    }

    #[test]
    fn small_sources_complete_in_the_head_build() {
        // Item: small files keep the old behaviour — the head build
        // is the whole build.
        for data in [&b""[..], b"one\ntwo\nthree", b"no newline at all"] {
            let index =
                LineIndex::build_prefix(data.len() as u64, INDEX_HEAD_BYTES, reader_for(data));
            assert!(index.is_complete(), "len {}", data.len());
            assert_eq!(
                index.checkpoints(),
                LineIndex::build_from_bytes(data).checkpoints()
            );
        }
    }

    #[test]
    fn forward_resolver_matches_bounded_within_and_beyond_the_prefix() {
        // 100-byte lines over ~2 MB: a head prefix covers only the
        // first part; the forward resolver must agree with the
        // bounded resolver on the full index for every line —
        // inside the prefix, past the frontier, and at the edges.
        let mut data = Vec::new();
        for _ in 0..20_000 {
            let mut line = vec![b'x'; 99];
            line.push(b'\n');
            data.extend_from_slice(&line);
        }
        let full = LineIndex::build_from_bytes(&data);
        let partial =
            LineIndex::build_prefix(data.len() as u64, INDEX_HEAD_BYTES / 4, reader_for(&data));
        assert!(!partial.is_complete());
        for line in [0u64, 1, 100, 5_000, 10_000, 19_999] {
            let want = full.line_start_offset(line, reader_for(&data));
            let got = partial.line_start_offset_forward(line, reader_for(&data));
            assert_eq!(got, want, "line {line}");
            assert_eq!(got, Some(line * 100));
        }
        // Past the last line: no phantom start at the end of a
        // newline-terminated source.
        assert_eq!(
            partial.line_start_offset_forward(20_000, reader_for(&data)),
            None
        );
        // A source without a trailing newline: the final partial
        // line resolves, and one past it does not.
        let mut data2 = data.clone();
        data2.truncate(data2.len() - 1); // last line loses its '\n'
        let partial2 =
            LineIndex::build_prefix(data2.len() as u64, INDEX_HEAD_BYTES / 4, reader_for(&data2));
        assert_eq!(
            partial2.line_start_offset_forward(19_999, reader_for(&data2)),
            Some(19_999 * 100)
        );
        assert_eq!(
            partial2.line_start_offset_forward(20_000, reader_for(&data2)),
            None
        );
    }

    /// Measured on this VM, not asserted: the staged-open timings
    /// over a ~1 GB in-memory source. Run with
    /// `cargo test --release bench_staged_open -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_staged_open_1gb() {
        // Log-like lines of 100 bytes, ~1 GB total.
        let mut data = Vec::with_capacity(1_050_000_000);
        let mut line = Vec::with_capacity(100);
        let mut n = 0u64;
        while data.len() < 1_000_000_000 {
            line.clear();
            line.extend_from_slice(
                format!("2026-11-01T00:00:00Z INFO worker request {n} ok").as_bytes(),
            );
            line.resize(99, b' ');
            line.push(b'\n');
            data.extend_from_slice(&line);
            n += 1;
        }
        let total = data.len() as u64;
        eprintln!("bench source: {total} bytes, {n} lines");

        // The old open path: a full synchronous build before the
        // first line question could be answered.
        let t = std::time::Instant::now();
        let full = LineIndex::build_from_bytes(&data);
        let full_ms = t.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "full build (old open): {full_ms:.1} ms ({:.0} MB/s)",
            total as f64 / (full_ms / 1000.0) / 1e6
        );

        // The staged open: head prefix only, then the editor can
        // paint; and the staged completion for comparison.
        let t = std::time::Instant::now();
        let head = LineIndex::build_prefix(total, INDEX_HEAD_BYTES, reader_for(&data));
        let head_ms = t.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "head prefix ({} bytes): {head_ms:.2} ms",
            head.indexed_bytes()
        );
        assert!(!head.is_complete());
        assert_eq!(
            head.checkpoints(),
            &full.checkpoints()[..head.checkpoints().len()]
        );

        let mut builder = LineIndexBuilder::new(total);
        let t = std::time::Instant::now();
        while !builder.advance(
            builder.indexed_bytes() + 32 * 1024 * 1024,
            reader_for(&data),
        ) {}
        let staged_ms = t.elapsed().as_secs_f64() * 1000.0;
        eprintln!("staged full build (32 MB steps): {staged_ms:.1} ms");
        assert_eq!(builder.finish().checkpoints(), full.checkpoints());
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
