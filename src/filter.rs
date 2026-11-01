//! Structural filter core (roadmap item 18): the model behind log
//! filter mode — the list of lines that contain a match, with exact
//! two-way mapping between filtered rows and full-file positions,
//! so the window can show only matching lines and jump back into
//! the full file at a line's exact byte offset with one click.
//! Nothing is written anywhere; the filter is a view model over the
//! document and its line index.
//!
//! A line belongs to the filter when a match **starts** inside it
//! (a match running past the line's end still counts for the line
//! it starts in). Matches come from the ordinary streaming search:
//! literal ([`crate::search::find_all`]) or masked
//! ([`crate::search::find_masked_all`]).
//!
//! # Partial indexes
//!
//! The filter is built against a line index. With a **complete**
//! index the whole document is searched and
//! [`FilterView::is_complete`] is true. With a **partial** (staged)
//! index the search is restricted to the indexed prefix
//! `[0, indexed_bytes)`: only matches lying fully inside the prefix
//! are considered (the scoped-find rule), [`FilterView::is_complete`]
//! is false, and [`FilterView::indexed_bytes`] reports how far the
//! filter reaches. Line byte ranges themselves are always exact —
//! a line found inside the prefix is read from the document, never
//! from the index — only the *discovery* of lines is bounded.

use crate::line_index::LineIndex;
use crate::piece_table::PieceTable;
use crate::search::{self, MaskedPattern, SearchOptions};

/// What the filter looks for.
#[derive(Clone, Copy, Debug)]
pub enum FilterQuery<'a> {
    /// A literal byte needle with the usual search options. An
    /// empty needle matches nothing (the streaming find's rule).
    Literal {
        needle: &'a [u8],
        options: SearchOptions,
    },
    /// A masked byte pattern (`4A ?? FF`, roadmap item 12).
    Masked(&'a MaskedPattern),
}

impl<'a> FilterQuery<'a> {
    pub fn literal(needle: &'a [u8], options: SearchOptions) -> Self {
        FilterQuery::Literal { needle, options }
    }

    pub fn masked(pattern: &'a MaskedPattern) -> Self {
        FilterQuery::Masked(pattern)
    }
}

/// One line of the filtered view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FilterLine {
    /// 0-based line number in the full document.
    pub line_no: u64,
    /// Byte offset of the line's first byte in the full document —
    /// the jump-back target.
    pub start: u64,
    /// Byte offset one past the line's last content byte: the
    /// line ending is excluded (a `'\r'` before the `'\n'` too),
    /// mirroring `Editor::line_content_range`.
    pub end: u64,
}

/// The filtered view: matching lines in document order, plus how
/// much of the document the search actually covered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilterView {
    lines: Vec<FilterLine>,
    complete: bool,
    indexed_bytes: u64,
    total_length: u64,
}

impl FilterView {
    /// The matching lines, in document order.
    pub fn lines(&self) -> &[FilterLine] {
        &self.lines
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// True when the search covered the whole document (the index
    /// was complete). When false, only the indexed prefix was
    /// searched — more lines may match past the frontier.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// How many bytes of the document the search covered: the
    /// whole document for a complete view, the indexed prefix for
    /// a partial one.
    pub fn indexed_bytes(&self) -> u64 {
        self.indexed_bytes
    }

    /// The document's total length the filter was built against.
    pub fn total_length(&self) -> u64 {
        self.total_length
    }

    /// Filtered row → full-file line: the [`FilterLine`] at
    /// 0-based row `row`, or `None` past the last row.
    pub fn line_at_row(&self, row: usize) -> Option<FilterLine> {
        self.lines.get(row).copied()
    }

    /// Full-file line → filtered row: the 0-based row at which
    /// `line_no` appears in the view, or `None` when the line did
    /// not match (or was past the searched prefix).
    pub fn row_of_line(&self, line_no: u64) -> Option<usize> {
        self.lines
            .binary_search_by_key(&line_no, |line| line.line_no)
            .ok()
    }
}

/// Build the filtered view of `table` for `query`, resolving line
/// positions through `index` (see the module docs for the
/// complete/partial contract). The table and the index must
/// describe the same bytes; a mismatched pair is a caller bug the
/// debug assertions in the index paths will surface.
pub fn build_filter(table: &PieceTable, index: &LineIndex, query: FilterQuery) -> FilterView {
    let complete = index.is_complete();
    let frontier = if complete {
        table.len()
    } else {
        index.indexed_bytes().min(table.len())
    };
    let matches = collect_matches(table, query, complete, frontier);

    // Map each match to its line, deduplicating lines (several
    // matches on one line list it once). Matches arrive in
    // ascending offset order, so line numbers ascend too. Only a
    // match's start matters here: a match that starts in a line
    // belongs to it even when it runs past the line's end.
    let mut lines: Vec<FilterLine> = Vec::new();
    let read_at = |offset: u64, out: &mut [u8]| table.read_into(offset, out);
    for m in matches {
        let line_no = line_of_offset(index, m, read_at);
        if lines
            .last()
            .is_some_and(|l: &FilterLine| l.line_no == line_no)
        {
            continue;
        }
        let Some(start) = index.line_start_offset(line_no, read_at) else {
            continue; // cannot happen for a match inside the source
        };
        let end = line_content_end(table, start);
        lines.push(FilterLine {
            line_no,
            start,
            end,
        });
    }
    FilterView {
        lines,
        complete,
        indexed_bytes: frontier,
        total_length: table.len(),
    }
}

/// Search the document (or its indexed prefix) for the query.
fn collect_matches(
    table: &PieceTable,
    query: FilterQuery,
    complete: bool,
    frontier: u64,
) -> Vec<u64> {
    match query {
        FilterQuery::Literal { needle, options } => {
            if complete {
                search::find_all(table, needle, &options)
            } else {
                search::find_in_range(table, needle, &options, 0, frontier)
            }
        }
        FilterQuery::Masked(pattern) => {
            let all = search::find_masked_all(table, pattern, None);
            if complete {
                all
            } else {
                // The masked find has no ranged form; restrict its
                // results to matches lying fully inside the prefix
                // (the same rule find_in_range applies).
                let len = pattern.len() as u64;
                all.into_iter().filter(|&m| m + len <= frontier).collect()
            }
        }
    }
}

/// The 0-based line containing byte `offset`: the nearest
/// checkpoint's line, plus the newlines between the checkpoint and
/// the offset, counted with a forward scan (bounded by the
/// checkpoint interval).
fn line_of_offset(
    index: &LineIndex,
    offset: u64,
    mut read_at: impl FnMut(u64, &mut [u8]) -> usize,
) -> u64 {
    let cp = index.nearest_checkpoint(offset);
    if cp.byte_offset >= offset {
        return cp.line_number;
    }
    let mut newlines = 0u64;
    let mut pos = cp.byte_offset;
    let mut buffer = [0u8; 8192];
    while pos < offset {
        let want = ((offset - pos).min(8192)) as usize;
        let read = read_at(pos, &mut buffer[..want]);
        if read == 0 {
            break;
        }
        newlines += crate::line_index::count_newlines_in(&buffer[..read]);
        pos += read as u64;
    }
    cp.line_number + newlines
}

/// The content end of the line starting at `start`: the offset of
/// its `'\n'`, minus a preceding `'\r'`, or the document's end.
fn line_content_end(table: &PieceTable, start: u64) -> u64 {
    let total = table.len();
    let mut pos = start;
    let mut buffer = [0u8; 8192];
    while pos < total {
        let want = ((total - pos).min(8192)) as usize;
        let read = table.read_into(pos, &mut buffer[..want]);
        if read == 0 {
            break;
        }
        if let Some(i) = buffer[..read].iter().position(|&b| b == b'\n') {
            let nl = pos + i as u64;
            return if nl > start && table.byte_at(nl - 1) == Some(b'\r') {
                nl - 1
            } else {
                nl
            };
        }
        pos += read as u64;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::line_index::LineIndexBuilder;
    use crate::search::parse_masked_pattern;

    /// A synthetic log: 10 lines, "ERROR" on lines 1, 4 and 7
    /// (0-based), two of them with the word twice.
    fn log() -> Vec<u8> {
        let mut data = Vec::new();
        for i in 0..10 {
            match i {
                1 => data.extend_from_slice(b"ERROR disk full\n"),
                4 => data.extend_from_slice(b"ERROR ERROR twice\n"),
                7 => data.extend_from_slice(b"an ERROR here\n"),
                _ => data.extend_from_slice(format!("ok line {i}\n").as_bytes()),
            }
        }
        data
    }

    fn view_for(data: &[u8], query: FilterQuery) -> FilterView {
        let table = PieceTable::from_original(data.to_vec());
        let index = LineIndex::build_from_bytes(data);
        build_filter(&table, &index, query)
    }

    #[test]
    fn literal_filter_lists_matching_lines_with_exact_ranges() {
        let data = log();
        let view = view_for(
            &data,
            FilterQuery::literal(b"ERROR", SearchOptions::default()),
        );
        assert!(view.is_complete());
        assert_eq!(view.indexed_bytes(), data.len() as u64);
        assert_eq!(view.len(), 3);
        let lines: Vec<u64> = view.lines().iter().map(|l| l.line_no).collect();
        assert_eq!(lines, vec![1, 4, 7]);
        // Ranges are the lines' own byte ranges (content, no '\n').
        for line in view.lines() {
            let start = LineIndex::build_from_bytes(&data)
                .line_start_offset(line.line_no, |o, out| {
                    let s = o as usize;
                    if s >= data.len() {
                        return 0;
                    }
                    let n = out.len().min(data.len() - s);
                    out[..n].copy_from_slice(&data[s..s + n]);
                    n
                })
                .unwrap();
            assert_eq!(line.start, start);
            assert_eq!(
                &data[line.start as usize..line.end as usize],
                match line.line_no {
                    1 => &b"ERROR disk full"[..],
                    4 => &b"ERROR ERROR twice"[..],
                    _ => &b"an ERROR here"[..],
                }
            );
        }
    }

    #[test]
    fn mapping_is_bidirectional() {
        let data = log();
        let view = view_for(
            &data,
            FilterQuery::literal(b"ERROR", SearchOptions::default()),
        );
        assert_eq!(view.row_of_line(1), Some(0));
        assert_eq!(view.row_of_line(4), Some(1));
        assert_eq!(view.row_of_line(7), Some(2));
        assert_eq!(view.row_of_line(0), None);
        assert_eq!(view.row_of_line(9), None);
        assert_eq!(view.line_at_row(0).unwrap().line_no, 1);
        assert_eq!(view.line_at_row(2).unwrap().line_no, 7);
        assert_eq!(view.line_at_row(3), None);
    }

    #[test]
    fn crlf_lines_report_content_without_the_cr() {
        let data = b"one\r\nERROR two\r\nthree\r\n";
        let view = view_for(
            data,
            FilterQuery::literal(b"ERROR", SearchOptions::default()),
        );
        assert_eq!(view.len(), 1);
        let line = view.line_at_row(0).unwrap();
        assert_eq!(line.line_no, 1);
        assert_eq!(&data[line.start as usize..line.end as usize], b"ERROR two");
    }

    #[test]
    fn masked_filter_finds_pattern_lines() {
        let mut data = Vec::new();
        data.extend_from_slice(b"\x00\x01\x02\x03\n");
        data.extend_from_slice(b"nothing here\n");
        data.extend_from_slice(b"\x4A\xFF\x10 tail\n");
        let pattern = parse_masked_pattern("4A ?? 10").unwrap();
        let view = view_for(&data, FilterQuery::masked(&pattern));
        assert_eq!(view.len(), 1);
        assert_eq!(view.line_at_row(0).unwrap().line_no, 2);
    }

    #[test]
    fn no_match_and_empty_needle_give_an_empty_view() {
        let data = log();
        let view = view_for(
            &data,
            FilterQuery::literal(b"ZZZ", SearchOptions::default()),
        );
        assert!(view.is_empty());
        assert!(view.is_complete());
        let view = view_for(&data, FilterQuery::literal(b"", SearchOptions::default()));
        assert!(view.is_empty());
    }

    #[test]
    fn partial_index_restricts_discovery_to_the_prefix() {
        // 100-byte lines over 1 MB; the prefix covers only the
        // first part, and "NEEDLE" sits before and after it.
        let mut data = Vec::new();
        for i in 0..10_000u32 {
            let mut line = format!("line {i:05}").into_bytes();
            if i == 100 || i == 9_000 {
                line = b"NEEDLE here".to_vec();
            }
            line.resize(99, b'.');
            line.push(b'\n');
            data.extend_from_slice(&line);
        }
        let table = PieceTable::from_original(data.clone());
        // A staged index covering roughly the first eighth.
        let mut builder = LineIndexBuilder::new(data.len() as u64);
        let reader = |offset: u64, out: &mut [u8]| {
            let s = offset as usize;
            if s >= data.len() {
                return 0;
            }
            let n = out.len().min(data.len() - s);
            out[..n].copy_from_slice(&data[s..s + n]);
            n
        };
        builder.advance(100_000, reader);
        let partial = builder.snapshot();
        assert!(!partial.is_complete());
        let view = build_filter(
            &table,
            &partial,
            FilterQuery::literal(b"NEEDLE", SearchOptions::default()),
        );
        // The line-100 needle is inside the prefix; line 9000 is not
        // discovered, and the view says it is incomplete.
        assert!(!view.is_complete());
        assert_eq!(view.indexed_bytes(), partial.indexed_bytes());
        assert_eq!(view.len(), 1);
        assert_eq!(view.line_at_row(0).unwrap().line_no, 100);
        // Against the complete index both needles are found.
        let full = LineIndex::build_from_bytes(&data);
        let view = build_filter(
            &table,
            &full,
            FilterQuery::literal(b"NEEDLE", SearchOptions::default()),
        );
        assert!(view.is_complete());
        assert_eq!(view.len(), 2);
    }
}
