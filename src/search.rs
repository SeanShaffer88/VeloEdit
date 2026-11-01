//! Streaming search and replace over the piece table (owner-confirmed v1
//! feature, 2026-10-31 — the blueprint does not mention search, so this
//! module is the specification of how it behaves).
//!
//! Search works on the *encoded byte level*: the caller encodes the
//! needle in the document's encoding (see
//! [`crate::encoding::encode_needle`]) and passes needle bytes. The
//! document itself is never materialised: matches are found by streaming
//! [`PieceTable::iter_slices`] and carrying the last `needle.len() - 1`
//! bytes of each slice into the next scan window, so a match that spans
//! two or three pieces — or is longer than any single piece — is found
//! exactly as if the document were contiguous.
//!
//! Semantics that callers (and the GUI, later) rely on:
//!
//! * [`find_all`] reports **overlapping** matches: searching for `aa` in
//!   `aaa` finds offsets 0 and 1. [`find_next`] is the same scan starting
//!   at an offset.
//! * An **empty needle never matches**: `find_all` returns an empty list
//!   and `find_next` returns `None`. (An empty needle would otherwise
//!   "match" at every byte offset, which is never what a user means.)
//! * **Case-insensitive** matching folds ASCII letters only (`A-Z` /
//!   `a-z`). Bytes outside ASCII — including the second byte of a UTF-8
//!   character — are never folded, so this is safe on UTF-8 and ANSI
//!   bytes. It is *not* Unicode case folding.
//! * [`replace_all`] uses **non-overlapping** matches, scanned left to
//!   right in the usual editor way: after a match is replaced, scanning
//!   continues after the replaced text, so `aa` in `aaa` is replaced
//!   once. The whole batch is applied as **one history entry**: a single
//!   undo restores the exact original piece layout and content. In
//!   case-insensitive mode each match occupies exactly the needle's byte
//!   length (the folded match has the same bytes counted, the replacement
//!   is inserted verbatim).
//! * **Scoped forms** ([`find_in_range`], [`count_in_range`],
//!   [`replace_all_in_range`], roadmap item 11) report or replace only
//!   matches lying fully inside `[start, end)`.
//! * **Windowed form** ([`find_in_window`], roadmap item 10) reports
//!   the matches *starting* inside `[start, end)` so a background
//!   worker can populate results window by window; concatenated over
//!   consecutive windows it equals [`find_all`].
//! * **Masked search** ([`MaskedPattern`], [`find_masked_all`], roadmap
//!   item 12) matches byte patterns with per-nibble wildcards.
//! * Scanning is **word-parallel** (roadmap item 8): candidate
//!   positions are filtered eight bytes at a time with SWAR bit
//!   tricks and verified individually, so results are identical to a
//!   naive scan — an equivalence test against one is in this module.
//!
//! # UTF-16 alignment caveat (read before searching UTF-16 files)
//!
//! Byte-level search over UTF-16 bytes can, in principle, match at an
//! offset that is *not* on a code-unit boundary: the needle's bytes could
//! appear straddling two adjacent characters (the second half of one
//! character plus the first half of the next). For needles built from
//! real text this is rare but possible. Callers presenting matches from a
//! UTF-16 document should treat a match at an odd offset relative to the
//! text start (after the BOM) as suspect. This caveat is recorded here
//! and in `docs/PARITY.md` rather than hidden; in practice the GUI will
//! filter to aligned offsets when it lands.

use crate::piece_table::{PieceNode, PieceTable};
use crate::undo::{EditAction, UndoStack};

/// Options shared by the find and replace operations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchOptions {
    /// Fold ASCII letters when comparing (see the module docs for the
    /// exact rule). Default: case-sensitive.
    pub case_insensitive: bool,
    /// Stop after this many matches in [`find_all`]. `None` means no
    /// limit. [`find_next`] always returns at most one match and ignores
    /// this field.
    pub max_results: Option<usize>,
}

/// The outcome of a [`replace_all`] batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplaceReport {
    /// How many ranges were replaced (0 when the needle did not occur).
    pub count: usize,
    /// The history action that was recorded, or `None` when nothing was
    /// replaced (no history entry is pushed for a no-op batch, so an
    /// undo after a failed replace-all still undoes the previous edit).
    pub action: Option<EditAction>,
}

/// Find every occurrence of `needle` in the document, in ascending
/// offset order, including overlapping occurrences. Returns an empty
/// list for an empty needle or no matches.
pub fn find_all(table: &PieceTable, needle: &[u8], options: &SearchOptions) -> Vec<u64> {
    let limit = options.max_results.unwrap_or(usize::MAX);
    collect_matches(table, needle, options, 0, table.len(), false, limit)
}

/// Find every occurrence of `needle` lying **fully inside**
/// `[start, end)` (roadmap item 11): a match that starts inside the
/// range but runs past `end` — or starts before `start` — is not
/// reported. Overlapping matches are reported, as in [`find_all`].
pub fn find_in_range(
    table: &PieceTable,
    needle: &[u8],
    options: &SearchOptions,
    start: u64,
    end: u64,
) -> Vec<u64> {
    let limit = options.max_results.unwrap_or(usize::MAX);
    let end = end.min(table.len());
    if start >= end {
        return Vec::new();
    }
    collect_matches(table, needle, options, start, end, false, limit)
}

/// How many occurrences of `needle` lie fully inside `[start, end)`.
/// Ignores `options.max_results` (a count is a count).
pub fn count_in_range(
    table: &PieceTable,
    needle: &[u8],
    options: &SearchOptions,
    start: u64,
    end: u64,
) -> usize {
    let unlimited = SearchOptions {
        max_results: None,
        ..*options
    };
    find_in_range(table, needle, &unlimited, start, end).len()
}

/// Find the first occurrence of `needle` at or after `from_offset`, or
/// `None`. An empty needle returns `None`.
pub fn find_next(
    table: &PieceTable,
    needle: &[u8],
    from_offset: u64,
    options: &SearchOptions,
) -> Option<u64> {
    collect_matches(table, needle, options, from_offset, table.len(), false, 1)
        .into_iter()
        .next()
}

/// Find every occurrence of `needle` whose **start** lies inside the
/// window `[start, end)` (roadmap item 10, incremental find-all core).
///
/// This is the windowed form a background worker drives piece by
/// piece: it scans only the window plus the `needle.len() - 1` bytes
/// past its end, and returns the matches that *start* in the window —
/// a match straddling the window's right edge is reported here (its
/// start is inside), while one straddling the left edge belongs to
/// the previous window. Concatenating the results over consecutive
/// windows `[0, w)`, `[w, 2w)`, … therefore reproduces
/// [`find_all`]'s list exactly, overlaps included — a property the
/// tests assert. `options.max_results` is ignored: each window
/// reports all of its matches and the caller enforces any overall
/// cap, the same split [`count_in_range`] uses.
pub fn find_in_window(
    table: &PieceTable,
    needle: &[u8],
    options: &SearchOptions,
    start: u64,
    end: u64,
) -> Vec<u64> {
    let n = needle.len();
    if n == 0 || start >= end || start >= table.len() {
        return Vec::new();
    }
    let folded_needle: Vec<u8> = if options.case_insensitive {
        needle.iter().map(|b| b.to_ascii_lowercase()).collect()
    } else {
        needle.to_vec()
    };
    // A match starting at `end - 1` reaches at most `n - 1` bytes
    // past the window; nothing beyond that can start in the window.
    let scan_end = end.saturating_add(n as u64 - 1).min(table.len());

    let mut results: Vec<u64> = Vec::new();
    // The same tail-carry scheme as collect_matches, but the first
    // window begins exactly at `start` (bytes before it are never
    // read: a match starting before `start` is not this window's).
    let mut tail: Vec<u8> = Vec::new();
    let mut tail_start: u64 = start;
    let mut slice_pos: u64 = 0;
    // Scan window, refilled per slice and reused across slices
    // (architectural audit): this used to be a fresh allocation of
    // tail + slice length for every piece the scan touched.
    let mut window: Vec<u8> = Vec::new();
    for slice in table.iter_slices() {
        let slice = slice.as_ref();
        let slice_end = slice_pos + slice.len() as u64;
        // The part of this slice overlapping [start, scan_end).
        let lo = start.max(slice_pos);
        let hi = scan_end.min(slice_end);
        if lo < hi {
            let part = &slice[(lo - slice_pos) as usize..(hi - slice_pos) as usize];
            window.clear();
            window.extend_from_slice(&tail);
            window.extend_from_slice(part);
            let window_start = tail_start;
            if window.len() >= n {
                for i in window_matches(&window, &folded_needle, options.case_insensitive) {
                    let absolute = window_start + i as u64;
                    if absolute >= start && absolute < end {
                        results.push(absolute);
                    }
                }
            }
            let window_end = window_start + window.len() as u64;
            let keep = (n - 1).min(window.len());
            tail = window[window.len() - keep..].to_vec();
            tail_start = window_end - keep as u64;
        }
        slice_pos = slice_end;
        if slice_pos >= scan_end {
            break;
        }
    }
    results
}

/// The shared streaming matcher behind [`find_all`] and [`find_next`].
///
/// `non_overlapping` is used by [`replace_all`]: once a match at offset
/// `p` is reported, any match starting before `p + needle.len()` is
/// suppressed. `limit` stops the scan early.
fn collect_matches(
    table: &PieceTable,
    needle: &[u8],
    options: &SearchOptions,
    from_offset: u64,
    end: u64,
    non_overlapping: bool,
    limit: usize,
) -> Vec<u64> {
    let n = needle.len();
    if n == 0 || limit == 0 {
        return Vec::new();
    }
    let folded_needle: Vec<u8> = if options.case_insensitive {
        needle.iter().map(|b| b.to_ascii_lowercase()).collect()
    } else {
        needle.to_vec()
    };

    let mut results: Vec<u64> = Vec::new();
    // Bytes carried from the previous window: the last `n - 1` bytes, so
    // a match crossing a piece boundary appears whole in the next window.
    // `tail_start` is the absolute offset of `tail[0]`, which is also the
    // first match start this window can newly report — the previous
    // window already covered every start before it, so nothing is
    // reported twice and nothing is skipped.
    let mut tail: Vec<u8> = Vec::new();
    let mut tail_start: u64 = 0;
    let mut suppress_until: u64 = 0;
    // Scan window, refilled per slice and reused across slices
    // (architectural audit): this used to be a fresh allocation of
    // tail + slice length for every piece the scan touched.
    let mut window: Vec<u8> = Vec::new();

    for slice in table.iter_slices() {
        let slice = slice.as_ref();
        if slice.is_empty() {
            continue;
        }
        window.clear();
        window.extend_from_slice(&tail);
        window.extend_from_slice(slice);
        let window_start = tail_start;

        if window.len() >= n {
            for i in window_matches(&window, &folded_needle, options.case_insensitive) {
                let absolute = window_start + i as u64;
                if absolute >= from_offset
                    && absolute >= suppress_until
                    && absolute + n as u64 <= end
                {
                    results.push(absolute);
                    if non_overlapping {
                        suppress_until = absolute + n as u64;
                    }
                    if results.len() >= limit {
                        return results;
                    }
                }
            }
        }

        let window_end = window_start + window.len() as u64;
        let keep = (n - 1).min(window.len());
        tail = window[window.len() - keep..].to_vec();
        tail_start = window_end - keep as u64;
    }
    results
}

/// All start positions in `window` where `folded_needle` matches, in
/// ascending order (roadmap item 8). The scan is word-parallel: SWAR
/// over 8-byte words finds the positions whose byte can be the
/// needle's first byte, and only those candidates are verified with
/// the full compare — the result list is identical to a naive
/// position-by-position scan, overlaps included.
fn window_matches(window: &[u8], folded_needle: &[u8], case_insensitive: bool) -> Vec<usize> {
    let n = folded_needle.len();
    if n == 0 || window.len() < n {
        return Vec::new();
    }
    let last_start = window.len() - n;
    let first = folded_needle[0];
    // A candidate byte must fold to `first`: it is either `first`
    // itself or, for a letter, its other case.
    let alt = if case_insensitive {
        first.to_ascii_uppercase()
    } else {
        first
    };
    let mut out: Vec<usize> = Vec::new();
    let mut base = 0usize;
    while base + 8 <= window.len() {
        let word = u64::from_le_bytes(window[base..base + 8].try_into().expect("8 bytes"));
        let mut hits = crate::line_index::swar_match_high_bits(word, first);
        if alt != first {
            hits |= crate::line_index::swar_match_high_bits(word, alt);
        }
        while hits != 0 {
            let bit = hits.trailing_zeros() as usize;
            hits &= hits - 1;
            let i = base + bit / 8;
            if i > last_start {
                break;
            }
            if bytes_match(window, i, folded_needle, case_insensitive) {
                out.push(i);
            }
        }
        base += 8;
    }
    for i in base..=last_start {
        let b = window[i];
        if (b == first || b == alt) && bytes_match(window, i, folded_needle, case_insensitive) {
            out.push(i);
        }
    }
    out
}

/// Compare the (possibly pre-folded) needle against `window` at `pos`.
fn bytes_match(window: &[u8], pos: usize, folded_needle: &[u8], case_insensitive: bool) -> bool {
    let hay = &window[pos..pos + folded_needle.len()];
    if case_insensitive {
        hay.iter()
            .zip(folded_needle.iter())
            .all(|(h, n)| h.to_ascii_lowercase() == *n)
    } else {
        hay == folded_needle
    }
}

/// A byte pattern with per-nibble wildcards (roadmap item 12), parsed
/// from text like `"4A ?? FF 00 2B"`: each token is two characters,
/// each character a hex digit or `?`. A `?` nibble matches anything,
/// so `"4?"` matches `0x40..=0x4F` and `"?F"` matches `0x0F, 0x1F, …`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaskedPattern {
    /// `(value, mask)` per byte: a document byte `b` matches when
    /// `b & mask == value & mask`.
    pairs: Vec<(u8, u8)>,
}

// Layout pin (architectural audit): the pattern's storage is exactly
// two bytes per pattern byte — the compact representation the masked
// scan's per-byte compare assumes. The tuple is not `repr(transparent)`
// (a tuple cannot carry the attribute); the pin states the same
// guarantee the attribute would: the element is `(u8, u8)` and nothing
// more, so `pairs` is a flat 2-bytes-per-byte array.
const _: () = assert!(std::mem::size_of::<(u8, u8)>() == 2);

impl MaskedPattern {
    /// Number of bytes in the pattern.
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// The `(value, mask)` pairs, in pattern order.
    pub fn pairs(&self) -> &[(u8, u8)] {
        &self.pairs
    }

    /// Does `bytes` (exactly the pattern's length) match?
    pub fn matches_at(&self, bytes: &[u8]) -> bool {
        bytes.len() == self.pairs.len()
            && self
                .pairs
                .iter()
                .zip(bytes.iter())
                .all(|((value, mask), b)| b & mask == value & mask)
    }
}

/// Parse a masked pattern from text (see [`MaskedPattern`]). Tokens
/// are separated by whitespace; every token must be exactly two
/// characters, each a hex digit or `?`. Returns a plain-words error.
pub fn parse_masked_pattern(text: &str) -> Result<MaskedPattern, String> {
    let mut pairs: Vec<(u8, u8)> = Vec::new();
    for token in text.split_whitespace() {
        let chars: Vec<char> = token.chars().collect();
        if chars.len() != 2 {
            return Err(format!(
                "pattern token '{token}' must be exactly two characters (a byte or ??)"
            ));
        }
        let mut value: u8 = 0;
        let mut mask: u8 = 0;
        for (pos, ch) in chars.iter().enumerate() {
            let shift = if pos == 0 { 4 } else { 0 };
            match ch {
                '?' => {}
                c => {
                    let digit = c
                        .to_digit(16)
                        .ok_or_else(|| format!("'{c}' is not a hex digit or ?"))?;
                    value |= (digit as u8) << shift;
                    mask |= 0xFu8 << shift;
                }
            }
        }
        pairs.push((value, mask));
    }
    if pairs.is_empty() {
        return Err("the pattern is empty".to_string());
    }
    Ok(MaskedPattern { pairs })
}

/// Find every occurrence of a masked pattern, streaming over the
/// piece table exactly like [`find_all`] (matches spanning pieces are
/// found; overlapping occurrences are reported). `max_results` caps
/// the list when set.
pub fn find_masked_all(
    table: &PieceTable,
    pattern: &MaskedPattern,
    max_results: Option<usize>,
) -> Vec<u64> {
    let n = pattern.len();
    if n == 0 {
        return Vec::new();
    }
    let limit = max_results.unwrap_or(usize::MAX);
    // Candidate anchor: the first fully-fixed byte, if the pattern has
    // one — its positions (found with the SWAR filter) are the only
    // possible match starts, offset back by the anchor's position.
    let anchor = pattern.pairs().iter().position(|(_, mask)| *mask == 0xFF);
    let mut results: Vec<u64> = Vec::new();
    let mut tail: Vec<u8> = Vec::new();
    let mut tail_start: u64 = 0;
    // Scan window and candidate-start list, refilled per slice and
    // reused across slices (architectural audit): both used to be
    // fresh allocations for every piece the scan touched.
    let mut window: Vec<u8> = Vec::new();
    let mut starts: Vec<usize> = Vec::new();
    for slice in table.iter_slices() {
        let slice = slice.as_ref();
        if slice.is_empty() {
            continue;
        }
        window.clear();
        window.extend_from_slice(&tail);
        window.extend_from_slice(slice);
        let window_start = tail_start;
        if window.len() >= n {
            let last_start = window.len() - n;
            starts.clear();
            match anchor {
                Some(a) => {
                    let target = pattern.pairs()[a].0;
                    let mut base = 0usize;
                    while base + 8 <= window.len() {
                        let word =
                            u64::from_le_bytes(window[base..base + 8].try_into().expect("8 bytes"));
                        let mut hits = crate::line_index::swar_match_high_bits(word, target);
                        while hits != 0 {
                            let bit = hits.trailing_zeros() as usize;
                            hits &= hits - 1;
                            let pos = base + bit / 8;
                            if pos >= a && pos - a <= last_start {
                                starts.push(pos - a);
                            }
                        }
                        base += 8;
                    }
                    for (pos, &b) in window.iter().enumerate().skip(base) {
                        if b == target && pos >= a && pos - a <= last_start {
                            starts.push(pos - a);
                        }
                    }
                }
                None => starts.extend(0..=last_start),
            }
            for &i in &starts {
                if pattern.matches_at(&window[i..i + n]) {
                    results.push(window_start + i as u64);
                    if results.len() >= limit {
                        return results;
                    }
                }
            }
        }
        let window_end = window_start + window.len() as u64;
        let keep = (n - 1).min(window.len());
        tail = window[window.len() - keep..].to_vec();
        tail_start = window_end - keep as u64;
    }
    results
}

/// Replace the `needle_len` bytes at `offset` with `replacement`,
/// recorded as a single undoable history entry, and return the recorded
/// action. Returns `None` (and changes nothing) when `needle_len` is 0
/// or `offset` is at or past the end of the document. A range running
/// past the end is clamped, exactly as
/// [`PieceTable::delete`] clamps.
pub fn replace_one(
    table: &mut PieceTable,
    history: &mut UndoStack,
    offset: u64,
    needle_len: u64,
    replacement: &[u8],
) -> Option<EditAction> {
    if needle_len == 0 || offset >= table.len() {
        return None;
    }
    let end = (offset + needle_len).min(table.len());
    let removed_len = end - offset;
    let replacement_len = replacement.len() as u64;
    let action = history.do_segment_replace(table, offset, end, |t| {
        let removed = t.delete(offset, removed_len);
        t.insert(offset, replacement);
        EditAction::Replace {
            offset,
            previous_pieces: removed,
            inserted_length: replacement_len,
            replacements: 1,
        }
    });
    Some(action)
}

/// Replace every non-overlapping occurrence of `needle` with
/// `replacement`, as ONE undoable history entry: a single undo restores
/// the exact original piece layout and content (see
/// [`UndoStack::do_segment_replace`]).
///
/// Matches are located against the original layout first, then applied
/// right-to-left so earlier match offsets stay valid while the document
/// changes length.
pub fn replace_all(
    table: &mut PieceTable,
    history: &mut UndoStack,
    needle: &[u8],
    replacement: &[u8],
    options: &SearchOptions,
) -> ReplaceReport {
    if needle.is_empty() {
        return ReplaceReport {
            count: 0,
            action: None,
        };
    }
    let matches = collect_matches(table, needle, options, 0, table.len(), true, usize::MAX);
    apply_replace_matches(table, history, &matches, needle.len() as u64, replacement)
}

/// Replace every non-overlapping occurrence of `needle` lying fully
/// inside `[start, end)` with `replacement`, as ONE undoable history
/// entry (roadmap item 11). Matches outside the range — including one
/// straddling either edge — are left exactly as they are.
pub fn replace_all_in_range(
    table: &mut PieceTable,
    history: &mut UndoStack,
    needle: &[u8],
    replacement: &[u8],
    options: &SearchOptions,
    start: u64,
    end: u64,
) -> ReplaceReport {
    if needle.is_empty() || start >= end {
        return ReplaceReport {
            count: 0,
            action: None,
        };
    }
    let end = end.min(table.len());
    let matches = collect_matches(table, needle, options, start, end, true, usize::MAX);
    apply_replace_matches(table, history, &matches, needle.len() as u64, replacement)
}

/// Apply a precomputed non-overlapping match list as one replace-all
/// history entry (shared by [`replace_all`] and
/// [`replace_all_in_range`]).
fn apply_replace_matches(
    table: &mut PieceTable,
    history: &mut UndoStack,
    matches: &[u64],
    needle_len: u64,
    replacement: &[u8],
) -> ReplaceReport {
    if matches.is_empty() {
        return ReplaceReport {
            count: 0,
            action: None,
        };
    }
    let count = matches.len();
    let first = matches[0];
    let last_end = matches[count - 1] + needle_len;
    let total_inserted = replacement.len() as u64 * count as u64;

    let action = history.do_segment_replace(table, first, last_end, |t| {
        let mut removed_groups: Vec<Vec<PieceNode>> = Vec::with_capacity(count);
        // Right-to-left: edits at higher offsets do not move the offsets
        // of matches that are still to be applied.
        for &m in matches.iter().rev() {
            let removed = t.delete(m, needle_len);
            removed_groups.push(removed);
            t.insert(m, replacement);
        }
        let previous_pieces: Vec<PieceNode> = removed_groups
            .into_iter()
            .rev()
            .flat_map(|group| group.into_iter())
            .collect();
        EditAction::Replace {
            offset: first,
            previous_pieces,
            inserted_length: total_inserted,
            replacements: count,
        }
    });
    ReplaceReport {
        count,
        action: Some(action),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::{encode_needle, TextEncoding};

    fn text(table: &PieceTable) -> String {
        String::from_utf8(table.read(0, table.len())).unwrap()
    }

    /// Document "abEFcd" over three pieces: original "abcd" with "EF"
    /// inserted at offset 2.
    fn three_piece_table() -> PieceTable {
        let mut table = PieceTable::from_original(b"abcd".to_vec());
        table.insert(2, b"EF");
        assert_eq!(table.piece_count(), 3);
        table
    }

    fn sensitive() -> SearchOptions {
        SearchOptions::default()
    }

    // ---- find: basics ----

    #[test]
    fn find_in_one_piece_and_no_match() {
        let table = PieceTable::from_original(b"hello world, hello".to_vec());
        assert_eq!(find_all(&table, b"hello", &sensitive()), vec![0, 13]);
        assert_eq!(find_all(&table, b"xyz", &sensitive()), Vec::<u64>::new());
        assert_eq!(
            find_all(&table, b"hello world, hello", &sensitive()),
            vec![0]
        );
    }

    #[test]
    fn find_empty_needle_and_empty_document() {
        let table = PieceTable::from_original(b"abc".to_vec());
        assert_eq!(find_all(&table, b"", &sensitive()), Vec::<u64>::new());
        assert_eq!(find_next(&table, b"", 0, &sensitive()), None);
        let empty = PieceTable::from_original(Vec::new());
        assert_eq!(find_all(&empty, b"a", &sensitive()), Vec::<u64>::new());
        assert_eq!(find_next(&empty, b"a", 0, &sensitive()), None);
    }

    #[test]
    fn find_needle_longer_than_document() {
        let table = PieceTable::from_original(b"ab".to_vec());
        assert_eq!(find_all(&table, b"abcdef", &sensitive()), Vec::<u64>::new());
    }

    // ---- find: across piece boundaries (the critical cases) ----

    #[test]
    fn find_spanning_two_pieces_at_the_boundary() {
        let table = three_piece_table(); // ab | EF | cd
                                         // "bE" starts in piece 1 and ends in piece 2.
        assert_eq!(find_all(&table, b"bE", &sensitive()), vec![1]);
        // "Fc" starts in piece 2 and ends in piece 3.
        assert_eq!(find_all(&table, b"Fc", &sensitive()), vec![3]);
    }

    #[test]
    fn find_spanning_three_pieces() {
        let table = three_piece_table(); // ab | EF | cd
                                         // "bEFc" starts in piece 1, covers piece 2 whole, ends in piece 3.
        assert_eq!(find_all(&table, b"bEFc", &sensitive()), vec![1]);
        // The whole document is one needle spanning all three pieces.
        assert_eq!(find_all(&table, b"abEFcd", &sensitive()), vec![0]);
    }

    #[test]
    fn find_needle_longer_than_any_single_piece() {
        // Build "abcd" as four separate one-byte pieces: inserting at
        // offset 0 never coalesces, so each insert stays its own piece.
        let mut table = PieceTable::from_original(Vec::new());
        for ch in *b"dcba" {
            table.insert(0, &[ch]);
        }
        assert_eq!(table.piece_count(), 4);
        assert_eq!(text(&table), "abcd");
        // The needle is 4 bytes; every piece is 1 byte.
        assert_eq!(find_all(&table, b"abcd", &sensitive()), vec![0]);
        assert_eq!(find_all(&table, b"bcd", &sensitive()), vec![1]);
    }

    #[test]
    fn find_repeated_pattern_across_many_small_pieces() {
        // Build "aaaa" as four one-byte pieces via non-coalescing inserts:
        // insert at offset 0 repeatedly prepends one piece per insert.
        let mut table = PieceTable::from_original(Vec::new());
        for _ in 0..4 {
            let len = table.len();
            table.insert(len, b"a");
        }
        // Typing at the end coalesces into one piece — that is fine too:
        assert_eq!(find_all(&table, b"aa", &sensitive()), vec![0, 1, 2]);
        // A genuinely fragmented version: separate originals + inserts.
        let mut frag = PieceTable::from_original(b"aa".to_vec());
        frag.insert(1, b"a"); // a | a(add) | a over three pieces
        assert_eq!(frag.piece_count(), 3);
        assert_eq!(find_all(&frag, b"aa", &sensitive()), vec![0, 1]);
    }

    #[test]
    fn find_overlapping_matches() {
        let table = PieceTable::from_original(b"aaa".to_vec());
        assert_eq!(find_all(&table, b"aa", &sensitive()), vec![0, 1]);
        let table = PieceTable::from_original(b"aaaa".to_vec());
        assert_eq!(find_all(&table, b"aa", &sensitive()), vec![0, 1, 2]);
    }

    // ---- find: options ----

    #[test]
    fn find_case_insensitive_ascii_only() {
        let table = PieceTable::from_original(b"AbC aBc abc ABC".to_vec());
        let ci = SearchOptions {
            case_insensitive: true,
            max_results: None,
        };
        assert_eq!(find_all(&table, b"abc", &ci), vec![0, 4, 8, 12]);
        assert_eq!(find_all(&table, b"ABC", &ci), vec![0, 4, 8, 12]);
        assert_eq!(find_all(&table, b"abc", &sensitive()), vec![8]);
    }

    #[test]
    fn find_max_results_caps_the_list() {
        let table = PieceTable::from_original(b"a a a a a".to_vec());
        let capped = SearchOptions {
            case_insensitive: false,
            max_results: Some(2),
        };
        assert_eq!(find_all(&table, b"a", &capped), vec![0, 2]);
        let zero = SearchOptions {
            case_insensitive: false,
            max_results: Some(0),
        };
        assert_eq!(find_all(&table, b"a", &zero), Vec::<u64>::new());
    }

    #[test]
    fn find_next_walks_matches() {
        let table = PieceTable::from_original(b"cat dog cat bird cat".to_vec());
        assert_eq!(find_next(&table, b"cat", 0, &sensitive()), Some(0));
        assert_eq!(find_next(&table, b"cat", 1, &sensitive()), Some(8));
        assert_eq!(find_next(&table, b"cat", 8, &sensitive()), Some(8));
        assert_eq!(find_next(&table, b"cat", 9, &sensitive()), Some(17));
        assert_eq!(find_next(&table, b"cat", 18, &sensitive()), None);
        assert_eq!(find_next(&table, b"cat", 999, &sensitive()), None);
    }

    #[test]
    fn find_next_across_a_piece_boundary() {
        let table = three_piece_table();
        assert_eq!(find_next(&table, b"bEFc", 0, &sensitive()), Some(1));
        assert_eq!(find_next(&table, b"bEFc", 2, &sensitive()), None);
    }

    // ---- find: UTF-16 needles ----

    #[test]
    fn find_in_utf16_encoded_document() {
        let bytes = crate::encoding::encode("hello world", TextEncoding::Utf16Le).unwrap();
        let table = PieceTable::from_original(bytes);
        let needle = encode_needle("ll", TextEncoding::Utf16Le).unwrap();
        assert_eq!(find_all(&table, &needle, &sensitive()), vec![4]);
        let needle = encode_needle("o w", TextEncoding::Utf16Le).unwrap();
        assert_eq!(find_all(&table, &needle, &sensitive()), vec![8]);
    }

    // ---- replace_one ----

    #[test]
    fn replace_one_and_undo_restores_layout_exactly() {
        let mut table = PieceTable::from_original(b"hello world".to_vec());
        let mut history = UndoStack::new();
        history.do_insert(&mut table, 5, b"---");
        let layout_before = table.pieces.clone();
        // "hello--- world": "world" starts at offset 9.
        let action = replace_one(&mut table, &mut history, 9, 5, b"there").unwrap();
        assert_eq!(text(&table), "hello--- there");
        match &action {
            EditAction::Replace {
                offset,
                inserted_length,
                replacements,
                ..
            } => {
                assert_eq!(*offset, 9);
                assert_eq!(*inserted_length, 5);
                assert_eq!(*replacements, 1);
            }
            other => panic!("expected a Replace action, got {other:?}"),
        }
        assert!(history.apply_undo(&mut table));
        assert_eq!(table.pieces, layout_before);
        assert_eq!(text(&table), "hello--- world");
        assert!(history.apply_redo(&mut table));
        assert_eq!(text(&table), "hello--- there");
        assert!(table.check_invariants());
    }

    #[test]
    fn replace_one_guards() {
        let mut table = PieceTable::from_original(b"abc".to_vec());
        let mut history = UndoStack::new();
        assert_eq!(replace_one(&mut table, &mut history, 0, 0, b"x"), None);
        assert_eq!(replace_one(&mut table, &mut history, 3, 1, b"x"), None);
        assert_eq!(replace_one(&mut table, &mut history, 99, 1, b"x"), None);
        assert_eq!(text(&table), "abc");
        assert!(!history.can_undo());
        // A range running past the end clamps.
        let action = replace_one(&mut table, &mut history, 1, 99, b"Z").unwrap();
        assert_eq!(text(&table), "aZ");
        assert!(matches!(action, EditAction::Replace { .. }));
    }

    #[test]
    fn replace_one_after_typed_text_undoes_cleanly() {
        // The coalescence hazard, directly: the replaced range starts
        // right after an Add piece that ends at the end of the append
        // buffer. The insert inside the replace must not silently extend
        // an unrecorded piece (see UndoStack::do_segment_replace).
        let mut table = PieceTable::from_original(Vec::new());
        let mut history = UndoStack::new();
        history.do_insert(&mut table, 0, b"AB");
        history.do_insert(&mut table, 0, b"Z");
        // Layout: Z (Add@2, len 1) | AB (Add@0, len 2); content "ZAB".
        assert_eq!(text(&table), "ZAB");
        assert_eq!(table.piece_count(), 2);
        let layout_before = table.pieces.clone();
        replace_one(&mut table, &mut history, 1, 2, b"W").unwrap();
        assert_eq!(text(&table), "ZW");
        assert!(history.apply_undo(&mut table));
        assert_eq!(table.pieces, layout_before);
        assert_eq!(text(&table), "ZAB");
        assert!(table.check_invariants());
    }

    // ---- replace_all ----

    #[test]
    fn replace_all_is_one_undo_entry() {
        let mut table = PieceTable::from_original(b"foo bar foo baz foo".to_vec());
        let mut history = UndoStack::new();
        let layout_before = table.pieces.clone();
        let report = replace_all(&mut table, &mut history, b"foo", b"quux", &sensitive());
        assert_eq!(report.count, 3);
        assert_eq!(text(&table), "quux bar quux baz quux");
        assert_eq!(history.undo_len(), 1);
        match &report.action {
            Some(EditAction::Replace {
                offset,
                inserted_length,
                replacements,
                previous_pieces,
            }) => {
                assert_eq!(*offset, 0);
                assert_eq!(*inserted_length, 12);
                assert_eq!(*replacements, 3);
                assert_eq!(previous_pieces.iter().map(|p| p.length).sum::<u64>(), 9);
            }
            other => panic!("expected a Replace action, got {other:?}"),
        }
        // ONE undo restores content and the exact original layout.
        assert!(history.apply_undo(&mut table));
        assert_eq!(text(&table), "foo bar foo baz foo");
        assert_eq!(table.pieces, layout_before);
        assert!(history.apply_redo(&mut table));
        assert_eq!(text(&table), "quux bar quux baz quux");
        assert!(table.check_invariants());
    }

    #[test]
    fn replace_all_no_match_records_nothing() {
        let mut table = PieceTable::from_original(b"hello".to_vec());
        let mut history = UndoStack::new();
        history.do_insert(&mut table, 5, b"!");
        let report = replace_all(&mut table, &mut history, b"zzz", b"q", &sensitive());
        assert_eq!(report.count, 0);
        assert_eq!(report.action, None);
        assert_eq!(history.undo_len(), 1); // only the earlier insert
                                           // An empty needle is also a no-op.
        let report = replace_all(&mut table, &mut history, b"", b"q", &sensitive());
        assert_eq!(report.count, 0);
        assert_eq!(history.undo_len(), 1);
    }

    #[test]
    fn replace_all_is_non_overlapping() {
        let mut table = PieceTable::from_original(b"aaa".to_vec());
        let mut history = UndoStack::new();
        let report = replace_all(&mut table, &mut history, b"aa", b"b", &sensitive());
        assert_eq!(report.count, 1);
        assert_eq!(text(&table), "ba");
    }

    #[test]
    fn replace_all_case_insensitive_and_empty_replacement() {
        let mut table = PieceTable::from_original(b"Foo fOO foo".to_vec());
        let mut history = UndoStack::new();
        let ci = SearchOptions {
            case_insensitive: true,
            max_results: None,
        };
        let report = replace_all(&mut table, &mut history, b"foo", b"bar", &ci);
        assert_eq!(report.count, 3);
        assert_eq!(text(&table), "bar bar bar");
        // Empty replacement deletes the matches, still one undo entry.
        let report = replace_all(&mut table, &mut history, b"bar ", b"", &sensitive());
        assert_eq!(report.count, 2);
        assert_eq!(text(&table), "bar");
        assert!(history.apply_undo(&mut table));
        assert_eq!(text(&table), "bar bar bar");
        assert!(table.check_invariants());
    }

    // ---- SWAR equivalence (roadmap item 8) ----

    /// The naive reference matcher: every position, full compare.
    fn naive_find(data: &[u8], needle: &[u8], ci: bool) -> Vec<u64> {
        if needle.is_empty() || data.len() < needle.len() {
            return Vec::new();
        }
        let fold = |b: u8| {
            if ci {
                b.to_ascii_lowercase()
            } else {
                b
            }
        };
        (0..=data.len() - needle.len())
            .filter(|&i| {
                data[i..i + needle.len()]
                    .iter()
                    .zip(needle.iter())
                    .all(|(h, n)| fold(*h) == fold(*n))
            })
            .map(|i| i as u64)
            .collect()
    }

    #[test]
    fn swar_scan_matches_naive_on_randomised_data() {
        let mut state = 0x1357_9BDF_2468_ACE0u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        // Small alphabet so matches (and near-misses) are dense.
        let alphabet = b"aAbB";
        for trial in 0..60 {
            let len = (next() % 300) as usize;
            let mut data: Vec<u8> = (0..len).map(|_| alphabet[(next() % 4) as usize]).collect();
            // Fragment the table: prepend single bytes so matches can
            // straddle piece and word boundaries alike.
            let mut table = PieceTable::from_original(std::mem::take(&mut data));
            if trial % 3 == 0 {
                table.insert(0, b"a");
                table.delete(0, 1);
                if table.len() > 4 {
                    table.insert(3, b"B");
                    table.delete(3, 1);
                }
            }
            let content = table.read(0, table.len());
            for needle in [&b"a"[..], b"ab", b"aAb", b"baab", b"bbbbbbbbbb"] {
                for ci in [false, true] {
                    let options = SearchOptions {
                        case_insensitive: ci,
                        max_results: None,
                    };
                    assert_eq!(
                        find_all(&table, needle, &options),
                        naive_find(&content, needle, ci),
                        "trial {trial} needle {needle:?} ci {ci}"
                    );
                }
            }
        }
        // A needle longer than any window chunk, planted at a word
        // boundary and at a piece boundary.
        let mut table = PieceTable::from_original(b"0123456789".to_vec());
        table.insert(5, b"ABCDEFGHIJ");
        assert_eq!(find_all(&table, b"4ABCDEFGHIJ5", &sensitive()), vec![4]);
        assert_eq!(find_all(&table, b"ABCDEFGHIJ", &sensitive()), vec![5]);
    }

    // ---- scoped search (roadmap item 11) ----

    #[test]
    fn find_in_range_requires_matches_fully_inside() {
        let table = PieceTable::from_original(b"foo bar foo baz foo".to_vec());
        // Whole-range behaves like find_all.
        assert_eq!(
            find_in_range(&table, b"foo", &sensitive(), 0, 19),
            vec![0, 8, 16]
        );
        // A range that cuts the last match off excludes it.
        assert_eq!(
            find_in_range(&table, b"foo", &sensitive(), 0, 18),
            vec![0, 8]
        );
        // A match starting before `start` is excluded even when most
        // of it lies inside.
        assert_eq!(
            find_in_range(&table, b"foo", &sensitive(), 1, 19),
            vec![8, 16]
        );
        assert_eq!(count_in_range(&table, b"foo", &sensitive(), 4, 15), 1);
        assert_eq!(count_in_range(&table, b"foo", &sensitive(), 9, 9), 0);
    }

    #[test]
    fn replace_all_in_range_is_one_undo_and_leaves_the_rest() {
        let mut table = PieceTable::from_original(b"foo bar foo baz foo".to_vec());
        let mut history = UndoStack::new();
        let layout_before = table.pieces.clone();
        // Replace only the middle occurrence (offsets 8..11).
        let report = replace_all_in_range(
            &mut table,
            &mut history,
            b"foo",
            b"quux",
            &sensitive(),
            4,
            15,
        );
        assert_eq!(report.count, 1);
        assert_eq!(text(&table), "foo bar quux baz foo");
        assert_eq!(history.undo_len(), 1);
        assert!(history.apply_undo(&mut table));
        assert_eq!(text(&table), "foo bar foo baz foo");
        assert_eq!(table.pieces, layout_before);
        // A range straddling a match replaces nothing.
        let report = replace_all_in_range(
            &mut table,
            &mut history,
            b"foo",
            b"quux",
            &sensitive(),
            1,
            3,
        );
        assert_eq!(report.count, 0);
        assert_eq!(text(&table), "foo bar foo baz foo");
        assert!(table.check_invariants());
    }

    // ---- masked search (roadmap item 12) ----

    #[test]
    fn masked_pattern_parsing() {
        let p = parse_masked_pattern("4A ?? FF 00 2B").unwrap();
        assert_eq!(p.len(), 5);
        assert_eq!(p.pairs()[0], (0x4A, 0xFF));
        assert_eq!(p.pairs()[1], (0x00, 0x00));
        // Nibble masks.
        let p = parse_masked_pattern("4? ?F").unwrap();
        assert_eq!(p.pairs(), &[(0x40, 0xF0), (0x0F, 0x0F)]);
        // Invalid forms are errors, not silent guesses.
        assert!(parse_masked_pattern("").is_err());
        assert!(parse_masked_pattern("4").is_err());
        assert!(parse_masked_pattern("4A2").is_err());
        assert!(parse_masked_pattern("ZZ").is_err());
        assert!(parse_masked_pattern("4G").is_err());
    }

    #[test]
    fn masked_search_finds_wildcard_and_nibble_matches() {
        let table = PieceTable::from_original(vec![
            0x4A, 0x11, 0xFF, 0x00, 0x2B, 0x99, 0x4A, 0x77, 0xFF, 0x00, 0x2B,
        ]);
        let p = parse_masked_pattern("4A ?? FF 00 2B").unwrap();
        assert_eq!(find_masked_all(&table, &p, None), vec![0, 6]);
        // Nibble masks: 4? matches 0x40..=0x4F, ?F matches any byte
        // ending in F.
        let table = PieceTable::from_original(vec![0x41, 0x2F, 0x4F, 0x3F, 0x51, 0x2F]);
        let p = parse_masked_pattern("4? ?F").unwrap();
        assert_eq!(find_masked_all(&table, &p, None), vec![0, 2]);
        // An all-wildcard pattern matches at every start.
        let p = parse_masked_pattern("?? ??").unwrap();
        assert_eq!(find_masked_all(&table, &p, None), vec![0, 1, 2, 3, 4]);
        // max_results caps the list.
        assert_eq!(find_masked_all(&table, &p, Some(2)), vec![0, 1]);
    }

    #[test]
    fn masked_search_spans_piece_boundaries() {
        let mut table = PieceTable::from_original(vec![0x01, 0x02, 0x05, 0x06]);
        table.insert(2, &[0x03, 0x04]);
        // Document: 01 02 | 03 04 | 05 06 over three pieces.
        let p = parse_masked_pattern("02 ?? 04 ?? 06").unwrap();
        assert_eq!(find_masked_all(&table, &p, None), vec![1]);
        let p = parse_masked_pattern("01 02 03 04 05 06").unwrap();
        assert_eq!(find_masked_all(&table, &p, None), vec![0]);
    }

    #[test]
    fn replace_all_across_pieces() {
        let mut table = three_piece_table(); // ab | EF | cd → "abEFcd"
        let mut history = UndoStack::new();
        let layout_before = table.pieces.clone();
        // The needle itself spans all three pieces.
        let report = replace_all(&mut table, &mut history, b"bEFc", b"X", &sensitive());
        assert_eq!(report.count, 1);
        assert_eq!(text(&table), "aXd");
        assert!(history.apply_undo(&mut table));
        assert_eq!(table.pieces, layout_before);
        assert_eq!(text(&table), "abEFcd");
    }

    /// (Item 10 core) Driving [`find_in_window`] over consecutive
    /// windows must reproduce [`find_all`] exactly, for every
    /// window size — matches are partitioned by their start, and
    /// straddlers at either edge are counted exactly once.
    #[test]
    fn windowed_find_concatenates_to_find_all() {
        // A multi-piece document with overlapping matches ("aa" in
        // "aaaaa"), a needle spanning pieces, and matches near both
        // ends: pieces "aa" | "aa" | "aXaaYaa".
        let mut table = PieceTable::from_original(b"aaaa".to_vec());
        table.insert(4, b"aXaaYaa");
        let doc = text(&table);
        assert_eq!(doc, "aaaaaXaaYaa");
        for (needle, options) in [
            (b"aa".as_ref(), sensitive()),
            (b"aaa".as_ref(), sensitive()),
            (b"aXa".as_ref(), sensitive()),
            (
                b"AA".as_ref(),
                SearchOptions {
                    case_insensitive: true,
                    max_results: None,
                },
            ),
        ] {
            let all = find_all(&table, needle, &options);
            assert!(!all.is_empty());
            for width in 1..=7u64 {
                let mut joined: Vec<u64> = Vec::new();
                let mut start = 0u64;
                while start < table.len() {
                    let end = (start + width).min(table.len());
                    joined.extend(find_in_window(&table, needle, &options, start, end));
                    start = end;
                }
                assert_eq!(joined, all, "needle {needle:?} width {width}");
                // One window covering the document also agrees.
                assert_eq!(
                    find_in_window(&table, needle, &options, 0, table.len()),
                    all,
                    "needle {needle:?} single window"
                );
            }
        }
    }

    /// (Item 10 core) A match straddling a window edge belongs to
    /// the window its start falls in; degenerate windows and
    /// needles report nothing.
    #[test]
    fn windowed_find_edge_cases() {
        let table = PieceTable::from_original(b"0123456789".to_vec());
        // "345" starts at 3 and straddles the [0, 4) / [4, 10) edge.
        assert_eq!(find_in_window(&table, b"345", &sensitive(), 0, 4), vec![3]);
        assert!(find_in_window(&table, b"345", &sensitive(), 4, 10).is_empty());
        // A window ending exactly at a match start excludes it; a
        // window starting there includes it.
        assert!(find_in_window(&table, b"34", &sensitive(), 0, 3).is_empty());
        assert_eq!(find_in_window(&table, b"34", &sensitive(), 3, 5), vec![3]);
        // Degenerate requests.
        assert!(find_in_window(&table, b"34", &sensitive(), 5, 5).is_empty());
        assert!(find_in_window(&table, b"34", &sensitive(), 10, 20).is_empty());
        assert!(find_in_window(&table, b"", &sensitive(), 0, 10).is_empty());
        // A window end past the document end is fine.
        assert_eq!(find_in_window(&table, b"89", &sensitive(), 8, 999), vec![8]);
    }
}
