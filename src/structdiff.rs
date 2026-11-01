//! Structural diff (roadmap item 43, slice R4c): compare two byte
//! sources block by block and report WHERE they differ, as
//! coalesced ranges — the complement of the byte-exact
//! [`crate::diff`], which answers "identical or not, and the first
//! differing byte" but walks away without a map of the damage.
//!
//! Each source is read through fixed windows and hashed block by
//! block with the hand-written SHA-256 ([`crate::hash::Sha256`]):
//! two blocks with equal digests are treated as equal, differing
//! digests mark the block. Consecutive differing blocks coalesce
//! into one region. The first divergence — the exact first byte
//! offset at which the sources disagree — is then found by a byte
//! comparison inside the first differing block, again windowed,
//! so memory stays flat however large the sources are.
//!
//! Blocks are compared at the same offsets in both sources. When
//! the sources have different lengths, the tail past the shorter
//! one is differing by definition and folds into the last region.

use crate::hash::{Sha256, StreamingHash};

/// The default block size: 1 MB.
pub const DEFAULT_BLOCK_SIZE: u64 = 1024 * 1024;

/// Read window inside a block for the first-divergence byte walk.
const WALK_WINDOW: usize = 64 * 1024;

/// One differing region: the byte range `[offset, offset + length)`
/// in which the two sources disagree. `length` is measured over
/// the longer source (the sources agree everywhere outside the
/// regions, over the shorter source's length).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffRegion {
    pub offset: u64,
    pub length: u64,
}

impl DiffRegion {
    /// One past the region's last byte.
    pub fn end(&self) -> u64 {
        self.offset + self.length
    }
}

/// The outcome of a structural comparison.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructDiffReport {
    /// Lengths of the two sources.
    pub len_a: u64,
    pub len_b: u64,
    /// The block size the comparison ran with.
    pub block_size: u64,
    /// Differing regions, in ascending offset order, coalesced.
    pub regions: Vec<DiffRegion>,
    /// The first byte offset at which the sources differ, or
    /// `None` when they are identical. When one source is a strict
    /// prefix of the other this is the shorter length.
    pub first_divergence: Option<u64>,
}

impl StructDiffReport {
    /// True when the sources are byte-identical.
    pub fn equal(&self) -> bool {
        self.regions.is_empty()
    }

    /// Total differing bytes (the sum of the region lengths).
    pub fn differing_bytes(&self) -> u64 {
        self.regions.iter().map(|r| r.length).sum()
    }
}

/// Hash one block of a source: read `[offset, offset + len)`
/// through `read_at` in windows and feed the SHA-256 state.
/// A short read (the source shrank or lied about its length) is
/// padded with zeros so the digest still differs from any honest
/// full block — a vanished tail must read as a difference, never
/// as a match.
fn hash_block(mut read_at: impl FnMut(u64, &mut [u8]) -> usize, offset: u64, len: u64) -> [u8; 32] {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; WALK_WINDOW];
    let mut pos = offset;
    let mut remaining = len;
    while remaining > 0 {
        let want = remaining.min(buffer.len() as u64) as usize;
        let read = read_at(pos, &mut buffer[..want]);
        if read == 0 {
            // Pad the rest of the block deterministically.
            hasher.update(&buffer[..want]);
            break;
        }
        let read = read.min(want);
        hasher.update(&buffer[..read]);
        pos += read as u64;
        remaining -= read as u64;
    }
    hasher.finalize()
}

/// Find the first byte offset in `[start, end)` where the two
/// sources disagree, comparing window by window. `end` may pass
/// the shorter source's length: bytes past it disagree by
/// definition, so a region that starts inside both sources but
/// runs past the shorter one's end reports the shorter length.
fn first_divergence_in(
    len_a: u64,
    mut read_a: impl FnMut(u64, &mut [u8]) -> usize,
    len_b: u64,
    mut read_b: impl FnMut(u64, &mut [u8]) -> usize,
    start: u64,
    end: u64,
) -> Option<u64> {
    let mut pos = start;
    let mut buf_a = vec![0u8; WALK_WINDOW];
    let mut buf_b = vec![0u8; WALK_WINDOW];
    while pos < end {
        let want = ((end - pos).min(WALK_WINDOW as u64)) as usize;
        let got_a = if pos < len_a {
            read_a(pos, &mut buf_a[..want]).min(want)
        } else {
            0
        };
        let got_b = if pos < len_b {
            read_b(pos, &mut buf_b[..want]).min(want)
        } else {
            0
        };
        let common = got_a.min(got_b);
        for i in 0..common {
            if buf_a[i] != buf_b[i] {
                return Some(pos + i as u64);
            }
        }
        if got_a != got_b {
            // One side ran out inside this window: the first byte
            // past the shorter side's data is the divergence.
            return Some(pos + common as u64);
        }
        if common == 0 {
            return None; // neither side can produce bytes: no divergence here
        }
        pos += common as u64;
    }
    None
}

/// Compare two byte sources block-wise (see the module docs).
/// `read_at(offset, buf) -> usize` follows the usual read
/// contract: fill `buf` from `offset`, return how many bytes were
/// filled (0 at or past the end). `block_size` of 0 is treated as
/// [`DEFAULT_BLOCK_SIZE`].
pub fn compare_sources(
    len_a: u64,
    mut read_a: impl FnMut(u64, &mut [u8]) -> usize,
    len_b: u64,
    mut read_b: impl FnMut(u64, &mut [u8]) -> usize,
    block_size: u64,
) -> StructDiffReport {
    let block_size = if block_size == 0 {
        DEFAULT_BLOCK_SIZE
    } else {
        block_size
    };
    let span = len_a.max(len_b);
    let mut regions: Vec<DiffRegion> = Vec::new();
    let mut open: Option<DiffRegion> = None;
    let close_open = |open: &mut Option<DiffRegion>, regions: &mut Vec<DiffRegion>| {
        if let Some(region) = open.take() {
            regions.push(region);
        }
    };
    let mut block_start = 0u64;
    while block_start < span {
        let block_len = block_size.min(span - block_start);
        // Each side hashes only the bytes it actually has; a side
        // past its end contributes an empty digest input, which
        // differs from any non-empty block — and two empty sides
        // cannot occur (the block lies inside the longer source).
        let digest_a = hash_block(
            &mut read_a,
            block_start,
            block_len.min(len_a.saturating_sub(block_start)),
        );
        let digest_b = hash_block(
            &mut read_b,
            block_start,
            block_len.min(len_b.saturating_sub(block_start)),
        );
        if digest_a == digest_b {
            close_open(&mut open, &mut regions);
        } else {
            match &mut open {
                Some(region) => region.length += block_len,
                None => {
                    open = Some(DiffRegion {
                        offset: block_start,
                        length: block_len,
                    })
                }
            }
        }
        block_start += block_len;
    }
    close_open(&mut open, &mut regions);
    let first_divergence = match regions.first() {
        None => None,
        Some(first) => first_divergence_in(len_a, read_a, len_b, read_b, first.offset, first.end()),
    };
    StructDiffReport {
        len_a,
        len_b,
        block_size,
        regions,
        first_divergence,
    }
}

/// A `read_at` closure over an in-memory byte string.
fn slice_reader(data: &[u8]) -> impl FnMut(u64, &mut [u8]) -> usize + '_ {
    move |offset: u64, out: &mut [u8]| -> usize {
        let start = offset as usize;
        if start >= data.len() {
            return 0;
        }
        let n = out.len().min(data.len() - start);
        out[..n].copy_from_slice(&data[start..start + n]);
        n
    }
}

/// Compare two in-memory byte strings (the test / small-input
/// convenience over [`compare_sources`]).
pub fn compare_bytes(a: &[u8], b: &[u8], block_size: u64) -> StructDiffReport {
    compare_sources(
        a.len() as u64,
        slice_reader(a),
        b.len() as u64,
        slice_reader(b),
        block_size,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_sources_have_no_regions() {
        let data = vec![7u8; 10_000];
        let report = compare_bytes(&data, &data, 1024);
        assert!(report.equal());
        assert_eq!(report.regions, Vec::new());
        assert_eq!(report.first_divergence, None);
        assert_eq!(report.differing_bytes(), 0);
        // Two empty sources are identical too.
        assert!(compare_bytes(&[], &[], 1024).equal());
    }

    #[test]
    fn a_single_differing_block_is_one_region() {
        let mut a = vec![0u8; 4096];
        let mut b = a.clone();
        b[1500] = 9;
        let report = compare_bytes(&a, &b, 1024);
        assert_eq!(
            report.regions,
            vec![DiffRegion {
                offset: 1024,
                length: 1024
            }]
        );
        assert_eq!(report.first_divergence, Some(1500));
        a[1500] = 9;
        assert!(compare_bytes(&a, &b, 1024).equal());
    }

    #[test]
    fn consecutive_differing_blocks_coalesce() {
        let a = vec![0u8; 4096];
        let mut b = a.clone();
        b[10] = 1; // block 0
        b[1030] = 1; // block 1
        b[3000] = 1; // block 2
        let report = compare_bytes(&a, &b, 1024);
        assert_eq!(
            report.regions,
            vec![DiffRegion {
                offset: 0,
                length: 3072
            }]
        );
        assert_eq!(report.first_divergence, Some(10));
        assert_eq!(report.differing_bytes(), 3072);
    }

    #[test]
    fn separated_differences_stay_separate_regions() {
        let a = vec![0u8; 8192];
        let mut b = a.clone();
        b[0] = 1;
        b[8000] = 1;
        let report = compare_bytes(&a, &b, 1024);
        assert_eq!(report.regions.len(), 2);
        assert_eq!(
            report.regions[0],
            DiffRegion {
                offset: 0,
                length: 1024
            }
        );
        assert_eq!(
            report.regions[1],
            DiffRegion {
                offset: 7168,
                length: 1024
            }
        );
        assert_eq!(report.first_divergence, Some(0));
    }

    #[test]
    fn a_longer_source_differs_in_the_tail() {
        let a = vec![5u8; 3000];
        let mut b = a.clone();
        b.extend_from_slice(&[5u8; 1500]);
        let report = compare_bytes(&a, &b, 1024);
        // Blocks 0..2 are shared; block 2 is short on side A
        // (952 bytes vs 1024) and block 3 exists only on side B:
        // the two fold into one tail region measured over B.
        assert_eq!(
            report.regions,
            vec![DiffRegion {
                offset: 2048,
                length: 2452
            }]
        );
        // A is a strict prefix of B: divergence at A's length.
        assert_eq!(report.first_divergence, Some(3000));
        // Symmetric argument order gives the same map.
        let flipped = compare_bytes(&b, &a, 1024);
        assert_eq!(flipped.regions, report.regions);
        assert_eq!(flipped.first_divergence, Some(3000));
    }

    #[test]
    fn block_size_changes_granularity_not_the_verdict() {
        let a: Vec<u8> = (0u8..=255).cycle().take(5000).collect();
        let mut b = a.clone();
        b[4999] = 0;
        for block in [1u64, 7, 64, 1000, 1_000_000] {
            let report = compare_bytes(&a, &b, block);
            assert!(!report.equal(), "block {block}");
            assert_eq!(report.first_divergence, Some(4999), "block {block}");
            // The single differing byte's block is the only region.
            assert_eq!(report.regions.len(), 1, "block {block}");
            assert!(report.regions[0].offset <= 4999 && 4999 < report.regions[0].end());
        }
        // A zero block size means the default.
        assert_eq!(compare_bytes(&a, &b, 0).block_size, DEFAULT_BLOCK_SIZE);
    }

    #[test]
    fn empty_against_nonempty_is_one_tail_region() {
        let report = compare_bytes(&[], &[1, 2, 3], 1024);
        assert_eq!(
            report.regions,
            vec![DiffRegion {
                offset: 0,
                length: 3
            }]
        );
        assert_eq!(report.first_divergence, Some(0));
    }
}
