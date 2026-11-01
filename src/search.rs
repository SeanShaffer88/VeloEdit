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
    collect_matches(table, needle, options, 0, false, limit)
}

/// Find the first occurrence of `needle` at or after `from_offset`, or
/// `None`. An empty needle returns `None`.
pub fn find_next(
    table: &PieceTable,
    needle: &[u8],
    from_offset: u64,
    options: &SearchOptions,
) -> Option<u64> {
    collect_matches(table, needle, options, from_offset, false, 1)
        .into_iter()
        .next()
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

    for slice in table.iter_slices() {
        let slice = slice.as_ref();
        if slice.is_empty() {
            continue;
        }
        let mut window: Vec<u8> = Vec::with_capacity(tail.len() + slice.len());
        window.extend_from_slice(&tail);
        window.extend_from_slice(slice);
        let window_start = tail_start;

        if window.len() >= n {
            let last_start = window.len() - n;
            let mut i = 0usize;
            while i <= last_start {
                if bytes_match(&window, i, &folded_needle, options.case_insensitive) {
                    let absolute = window_start + i as u64;
                    if absolute >= from_offset && absolute >= suppress_until {
                        results.push(absolute);
                        if non_overlapping {
                            suppress_until = absolute + n as u64;
                        }
                        if results.len() >= limit {
                            return results;
                        }
                    }
                }
                i += 1;
            }
        }

        let window_end = window_start + window.len() as u64;
        let keep = (n - 1).min(window.len());
        tail = window[window.len() - keep..].to_vec();
        tail_start = window_end - keep as u64;
    }
    results
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
    let matches = collect_matches(table, needle, options, 0, true, usize::MAX);
    if matches.is_empty() {
        return ReplaceReport {
            count: 0,
            action: None,
        };
    }
    let count = matches.len();
    let first = matches[0];
    let needle_len = needle.len() as u64;
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
}
