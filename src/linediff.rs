//! Line diff core: compare two line sources and report where
//! they differ, as coalesced line regions — the alignment the
//! side-by-side streaming diff paints from.
//!
//! Each source is a line count plus a `FnMut(u64, &mut Vec<u8>)
//! -> bool` reader that fills the buffer with one line's content
//! (without its `\n` terminator; a `\r` before it stays — the
//! comparison is byte-honest, so a CRLF file and an LF file
//! differ on every line) and returns false past the last line.
//! [`indexed_line_reader`] adapts a [`crate::line_index::LineIndex`]
//! plus a byte reader to that shape.
//!
//! # The algorithm, honestly
//!
//! This is **anchor-and-trim alignment, not a minimal Myers
//! edit script.** It walks both sources together over their
//! common prefix and suffix; in the differing middle it consumes
//! equal lines in lockstep, and when the lines under the two
//! cursors disagree it searches a bounded window ahead (line
//! hashes in a map, candidates confirmed by exact comparison of
//! a few lines) for the next pair of lines that agree — an
//! anchor — emits the skipped span as a region, and resyncs
//! there. Regions are then tightened by trimming equal lines
//! off both ends of the span, and adjacent regions merge.
//!
//! Consequences, stated plainly: the regions always cover every
//! real difference and never cover a line pair that is equal at
//! both region ends after trimming, but a region's interior can
//! contain equal lines the anchor search stepped over, and the
//! alignment is not guaranteed minimal (a line moved across an
//! anchor window reads as a delete plus an insert). Memory is
//! bounded: two line buffers, plus one window of line hashes
//! ([`ANCHOR_WINDOW`] digests) per anchor search — the sources
//! themselves are streamed, never held.

use std::collections::HashMap;

use crate::hash::{Sha256, StreamingHash};
use crate::line_index::LineIndex;

/// Lines of one side hashed per anchor search (see the module
/// docs for the cost this bounds).
pub const ANCHOR_WINDOW: u64 = 1024;

/// Lines that must agree, exactly, before a hash candidate is
/// accepted as an anchor (fewer at the sources' ends).
pub const ANCHOR_CONFIRM: u64 = 3;

/// One differing region: the left lines
/// `[left_start, left_start + left_count)` correspond to the
/// right lines `[right_start, right_start + right_count)`.
/// A zero count is a pure insertion (left) or deletion (right).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineDiffRegion {
    pub left_start: u64,
    pub left_count: u64,
    pub right_start: u64,
    pub right_count: u64,
}

impl LineDiffRegion {
    /// One past the region's last left line.
    pub fn left_end(&self) -> u64 {
        self.left_start + self.left_count
    }

    /// One past the region's last right line.
    pub fn right_end(&self) -> u64 {
        self.right_start + self.right_count
    }
}

/// The outcome of a line comparison.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineDiffReport {
    /// Line counts of the two sources.
    pub left_lines: u64,
    pub right_lines: u64,
    /// Differing regions in ascending order, coalesced.
    pub regions: Vec<LineDiffRegion>,
    /// The left line index at which the first region begins, or
    /// `None` when the sources are line-identical. For a pure
    /// insertion this is the line before which the new lines
    /// belong.
    pub first_divergence_line: Option<u64>,
}

impl LineDiffReport {
    /// True when the sources are line-identical.
    pub fn equal(&self) -> bool {
        self.regions.is_empty()
    }
}

/// The comparison state: the two readers, their counts, and the
/// two reusable line buffers (a line is materialised only to be
/// compared or hashed, then the buffer is reused).
struct Cmp<A, B>
where
    A: FnMut(u64, &mut Vec<u8>) -> bool,
    B: FnMut(u64, &mut Vec<u8>) -> bool,
{
    left_count: u64,
    right_count: u64,
    read_left: A,
    read_right: B,
    buf_left: Vec<u8>,
    buf_right: Vec<u8>,
}

impl<A, B> Cmp<A, B>
where
    A: FnMut(u64, &mut Vec<u8>) -> bool,
    B: FnMut(u64, &mut Vec<u8>) -> bool,
{
    fn line_eq(&mut self, left: u64, right: u64) -> bool {
        if !(self.read_left)(left, &mut self.buf_left) {
            return false;
        }
        if !(self.read_right)(right, &mut self.buf_right) {
            return false;
        }
        self.buf_left == self.buf_right
    }

    fn hash_left(&mut self, line: u64) -> [u8; 32] {
        (self.read_left)(line, &mut self.buf_left);
        let mut hasher = Sha256::new();
        hasher.update(&self.buf_left);
        hasher.finalize()
    }

    fn hash_right(&mut self, line: u64) -> [u8; 32] {
        (self.read_right)(line, &mut self.buf_right);
        let mut hasher = Sha256::new();
        hasher.update(&self.buf_right);
        hasher.finalize()
    }

    /// Trim equal lines off both ends of a span pair and push
    /// what remains as a region, merging with the previous
    /// region when the two touch.
    fn push_region(
        &mut self,
        regions: &mut Vec<LineDiffRegion>,
        mut left_start: u64,
        mut left_end: u64,
        mut right_start: u64,
        mut right_end: u64,
    ) {
        while left_start < left_end
            && right_start < right_end
            && self.line_eq(left_start, right_start)
        {
            left_start += 1;
            right_start += 1;
        }
        while left_end > left_start
            && right_end > right_start
            && self.line_eq(left_end - 1, right_end - 1)
        {
            left_end -= 1;
            right_end -= 1;
        }
        if left_end == left_start && right_end == right_start {
            return;
        }
        let region = LineDiffRegion {
            left_start,
            left_count: left_end - left_start,
            right_start,
            right_count: right_end - right_start,
        };
        if let Some(last) = regions.last_mut() {
            if last.left_end() == region.left_start && last.right_end() == region.right_start {
                last.left_count += region.left_count;
                last.right_count += region.right_count;
                return;
            }
        }
        regions.push(region);
    }

    /// Confirm an anchor candidate: the lines at `left`/`right`
    /// and the next few after them must be exactly equal (as
    /// many as both sources can supply, at least one).
    fn anchor_confirmed(&mut self, left: u64, right: u64, left_end: u64, right_end: u64) -> bool {
        let mut k = 0;
        while k < ANCHOR_CONFIRM && left + k < left_end && right + k < right_end {
            if !self.line_eq(left + k, right + k) {
                return false;
            }
            k += 1;
        }
        k > 0
    }

    /// Search for the next anchor pair at or after
    /// `(left, right)`: a pair of equal lines from which the
    /// lockstep walk can resume. Both directions are tried (map
    /// the left window, scan the right; then the reverse).
    fn find_anchor(
        &mut self,
        left: u64,
        left_end: u64,
        right: u64,
        right_end: u64,
    ) -> Option<(u64, u64)> {
        // Map the left window's hashes, scan the right.
        let left_span = (left_end - left).min(ANCHOR_WINDOW);
        let mut map: HashMap<[u8; 32], u64> = HashMap::with_capacity(left_span as usize);
        for k in 0..left_span {
            let digest = self.hash_left(left + k);
            map.entry(digest).or_insert(left + k);
        }
        let right_span = (right_end - right).min(ANCHOR_WINDOW);
        for k in 0..right_span {
            let digest = self.hash_right(right + k);
            if let Some(&candidate) = map.get(&digest) {
                if self.anchor_confirmed(candidate, right + k, left_end, right_end) {
                    return Some((candidate, right + k));
                }
            }
        }
        // Map the right window's hashes, scan the left.
        let mut map: HashMap<[u8; 32], u64> = HashMap::with_capacity(right_span as usize);
        for k in 0..right_span {
            let digest = self.hash_right(right + k);
            map.entry(digest).or_insert(right + k);
        }
        for k in 0..left_span {
            let digest = self.hash_left(left + k);
            if let Some(&candidate) = map.get(&digest) {
                if self.anchor_confirmed(left + k, candidate, left_end, right_end) {
                    return Some((left + k, candidate));
                }
            }
        }
        None
    }

    fn run(&mut self) -> Vec<LineDiffRegion> {
        let mut regions = Vec::new();
        // Common prefix and suffix first: they frame the middle
        // and cover the frequent "one small change" case without
        // any anchor search at all.
        let (mut i, mut j) = (0u64, 0u64);
        while i < self.left_count && j < self.right_count && self.line_eq(i, j) {
            i += 1;
            j += 1;
        }
        let (mut left_end, mut right_end) = (self.left_count, self.right_count);
        while left_end > i && right_end > j && self.line_eq(left_end - 1, right_end - 1) {
            left_end -= 1;
            right_end -= 1;
        }
        // The middle. Each pass consumes equal lines in lockstep,
        // then anchors past one differing span.
        while i < left_end || j < right_end {
            while i < left_end && j < right_end && self.line_eq(i, j) {
                i += 1;
                j += 1;
            }
            if i >= left_end && j >= right_end {
                break;
            }
            if i >= left_end || j >= right_end {
                // One side is spent: the rest of the other side
                // is one final region.
                self.push_region(&mut regions, i, left_end, j, right_end);
                break;
            }
            match self.find_anchor(i, left_end, j, right_end) {
                Some((anchor_left, anchor_right)) => {
                    self.push_region(&mut regions, i, anchor_left, j, anchor_right);
                    i = anchor_left;
                    j = anchor_right;
                }
                None => {
                    self.push_region(&mut regions, i, left_end, j, right_end);
                    break;
                }
            }
        }
        regions
    }
}

/// Compare two line sources (see the module docs for the reader
/// contract and the algorithm's honest limits).
pub fn compare_lines<A, B>(
    left_count: u64,
    read_left: A,
    right_count: u64,
    read_right: B,
) -> LineDiffReport
where
    A: FnMut(u64, &mut Vec<u8>) -> bool,
    B: FnMut(u64, &mut Vec<u8>) -> bool,
{
    let mut cmp = Cmp {
        left_count,
        right_count,
        read_left,
        read_right,
        buf_left: Vec::new(),
        buf_right: Vec::new(),
    };
    let regions = cmp.run();
    LineDiffReport {
        left_lines: left_count,
        right_lines: right_count,
        first_divergence_line: regions.first().map(|r| r.left_start),
        regions,
    }
}

/// Compare two in-memory line lists (the test / small-input
/// convenience over [`compare_lines`]).
pub fn compare_line_vecs(left: &[Vec<u8>], right: &[Vec<u8>]) -> LineDiffReport {
    let read = |lines: &[Vec<u8>], index: u64, out: &mut Vec<u8>| -> bool {
        match lines.get(index as usize) {
            Some(line) => {
                out.clear();
                out.extend_from_slice(line);
                true
            }
            None => false,
        }
    };
    compare_lines(
        left.len() as u64,
        |index: u64, out: &mut Vec<u8>| read(left, index, out),
        right.len() as u64,
        |index: u64, out: &mut Vec<u8>| read(right, index, out),
    )
}

/// Adapt a complete [`LineIndex`] and a byte reader (the
/// `read_at` closure of [`LineIndex::build`]) into the
/// line-source shape [`compare_lines`] consumes: returns the
/// line count and a reader yielding each line's content without
/// its `\n`. The index must be complete — for a partial index
/// its `line_count` describes only the indexed prefix, and that
/// prefix is what would be compared.
pub fn indexed_line_reader<'a, F>(
    index: &'a LineIndex,
    mut read_at: F,
) -> (u64, impl FnMut(u64, &mut Vec<u8>) -> bool + 'a)
where
    F: FnMut(u64, &mut [u8]) -> usize + 'a,
{
    let count = index.line_count();
    let total = index.total_length();
    let reader = move |line: u64, out: &mut Vec<u8>| -> bool {
        let Some(start) = index.line_start_offset(line, &mut read_at) else {
            return false;
        };
        let end = match index.line_start_offset(line + 1, &mut read_at) {
            // The next line starts one past this line's '\n'.
            Some(next) => next.saturating_sub(1).max(start),
            None => total,
        };
        out.clear();
        let mut pos = start;
        let mut chunk = [0u8; 64 * 1024];
        while pos < end {
            let want = ((end - pos).min(chunk.len() as u64)) as usize;
            let got = read_at(pos, &mut chunk[..want]);
            if got == 0 {
                return false;
            }
            let got = got.min(want);
            out.extend_from_slice(&chunk[..got]);
            pos += got as u64;
        }
        true
    };
    (count, reader)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<Vec<u8>> {
        text.split('\n').map(|s| s.as_bytes().to_vec()).collect()
    }

    fn numbered(count: u64) -> Vec<Vec<u8>> {
        (0..count)
            .map(|i| format!("line {i}").into_bytes())
            .collect()
    }

    #[test]
    fn identical_sources_have_no_regions() {
        let a = numbered(100);
        let report = compare_line_vecs(&a, &a);
        assert!(report.equal());
        assert_eq!(report.regions, Vec::new());
        assert_eq!(report.first_divergence_line, None);
        assert!(compare_line_vecs(&[], &[]).equal());
    }

    #[test]
    fn a_changed_line_is_one_tight_region() {
        let a = lines("one\ntwo\nthree\nfour\nfive");
        let b = lines("one\ntwo\nTHREE\nfour\nfive");
        let report = compare_line_vecs(&a, &b);
        assert_eq!(
            report.regions,
            vec![LineDiffRegion {
                left_start: 2,
                left_count: 1,
                right_start: 2,
                right_count: 1
            }]
        );
        assert_eq!(report.first_divergence_line, Some(2));
    }

    #[test]
    fn an_insertion_is_a_zero_left_count_region() {
        let a = numbered(50);
        let mut b = a.clone();
        b.insert(10, b"inserted one".to_vec());
        b.insert(11, b"inserted two".to_vec());
        let report = compare_line_vecs(&a, &b);
        assert_eq!(
            report.regions,
            vec![LineDiffRegion {
                left_start: 10,
                left_count: 0,
                right_start: 10,
                right_count: 2
            }]
        );
        assert_eq!(report.first_divergence_line, Some(10));
        // And read the other way it is a deletion.
        let flipped = compare_line_vecs(&b, &a);
        assert_eq!(
            flipped.regions,
            vec![LineDiffRegion {
                left_start: 10,
                left_count: 2,
                right_start: 10,
                right_count: 0
            }]
        );
    }

    #[test]
    fn a_deletion_in_the_middle_resyncs() {
        let a = numbered(200);
        let mut b = a.clone();
        b.drain(100..105);
        let report = compare_line_vecs(&a, &b);
        assert_eq!(
            report.regions,
            vec![LineDiffRegion {
                left_start: 100,
                left_count: 5,
                right_start: 100,
                right_count: 0
            }]
        );
    }

    #[test]
    fn changes_at_both_ends_are_two_regions() {
        let mut a = numbered(100);
        let mut b = a.clone();
        b[0] = b"changed first".to_vec();
        b[99] = b"changed last".to_vec();
        a[50] = b"same".to_vec();
        b[50] = b"same".to_vec();
        let report = compare_line_vecs(&a, &b);
        assert_eq!(report.regions.len(), 2);
        assert_eq!(report.regions[0].left_start, 0);
        assert_eq!(report.regions[0].left_count, 1);
        assert_eq!(report.regions[1].left_start, 99);
        assert_eq!(report.first_divergence_line, Some(0));
    }

    #[test]
    fn far_apart_changes_stay_separate_near_changes_coalesce() {
        // Two changes with a long run of equal lines between
        // them: two regions.
        let a = numbered(5000);
        let mut b = a.clone();
        b[10] = b"changed A".to_vec();
        b[4000] = b"changed B".to_vec();
        let report = compare_line_vecs(&a, &b);
        assert_eq!(report.regions.len(), 2, "{:?}", report.regions);
        assert_eq!(report.regions[0].left_start, 10);
        assert_eq!(report.regions[1].left_start, 4000);
        // Two changes a few lines apart: the anchor walk steps
        // over the small equal gap... it does not — the lockstep
        // consumes it and the regions stay separate but tight.
        // Changes on ADJACENT lines, though, are one region.
        let mut c = a.clone();
        c[10] = b"changed A".to_vec();
        c[11] = b"changed B".to_vec();
        let report = compare_line_vecs(&a, &c);
        assert_eq!(
            report.regions,
            vec![LineDiffRegion {
                left_start: 10,
                left_count: 2,
                right_start: 10,
                right_count: 2
            }]
        );
    }

    #[test]
    fn wholly_different_sources_are_one_region() {
        let a = lines("a\nb\nc");
        let b = lines("x\ny\nz\nw");
        let report = compare_line_vecs(&a, &b);
        assert_eq!(
            report.regions,
            vec![LineDiffRegion {
                left_start: 0,
                left_count: 3,
                right_start: 0,
                right_count: 4
            }]
        );
        assert_eq!(report.first_divergence_line, Some(0));
    }

    #[test]
    fn one_side_empty() {
        let a = lines("a\nb");
        let report = compare_line_vecs(&a, &[]);
        assert_eq!(
            report.regions,
            vec![LineDiffRegion {
                left_start: 0,
                left_count: 2,
                right_start: 0,
                right_count: 0
            }]
        );
    }

    #[test]
    fn prefix_and_suffix_only_difference() {
        // B is A with a line added at the very top and bottom.
        let a = numbered(30);
        let mut b = vec![b"new first".to_vec()];
        b.extend(a.iter().cloned());
        b.push(b"new last".to_vec());
        let report = compare_line_vecs(&a, &b);
        assert_eq!(
            report.regions,
            vec![
                LineDiffRegion {
                    left_start: 0,
                    left_count: 0,
                    right_start: 0,
                    right_count: 1
                },
                LineDiffRegion {
                    left_start: 30,
                    left_count: 0,
                    right_start: 31,
                    right_count: 1
                },
            ]
        );
    }

    #[test]
    fn indexed_reader_matches_the_vec_reader() {
        let text = b"alpha\nbeta\ngamma\ndelta";
        let other = b"alpha\nBETA\ngamma\ndelta\nextra";
        let index_a = LineIndex::build_from_bytes(text);
        let index_b = LineIndex::build_from_bytes(other);
        let (count_a, read_a) = indexed_line_reader(&index_a, |offset: u64, out: &mut [u8]| {
            let start = offset as usize;
            if start >= text.len() {
                return 0;
            }
            let n = out.len().min(text.len() - start);
            out[..n].copy_from_slice(&text[start..start + n]);
            n
        });
        let (count_b, read_b) = indexed_line_reader(&index_b, |offset: u64, out: &mut [u8]| {
            let start = offset as usize;
            if start >= other.len() {
                return 0;
            }
            let n = out.len().min(other.len() - start);
            out[..n].copy_from_slice(&other[start..start + n]);
            n
        });
        assert_eq!(count_a, 4);
        assert_eq!(count_b, 5);
        let report = compare_lines(count_a, read_a, count_b, read_b);
        let expected = compare_line_vecs(
            &lines("alpha\nbeta\ngamma\ndelta"),
            &lines("alpha\nBETA\ngamma\ndelta\nextra"),
        );
        assert_eq!(report, expected);
        assert_eq!(
            report.regions,
            vec![
                LineDiffRegion {
                    left_start: 1,
                    left_count: 1,
                    right_start: 1,
                    right_count: 1
                },
                LineDiffRegion {
                    left_start: 4,
                    left_count: 0,
                    right_start: 4,
                    right_count: 1
                },
            ]
        );
    }
}
