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

/// An incremental filter build (slice R4c, item 13): the
/// background form of [`build_filter`]. The worker drives
/// [`FilterBuilder::advance`] window by window over a snapshot of
/// the document; each call folds the matches starting inside the
/// new window into the line list, and [`FilterBuilder::snapshot`]
/// hands the UI thread the view built so far. Concatenated over
/// consecutive windows the result equals [`build_filter`] over a
/// complete index exactly — the windowed find concatenates to the
/// whole-document find (see `crate::search`), and line numbers
/// come from counting newlines forward from position 0, which is
/// what the index's checkpoints summarise.
///
/// No line index is involved at all, so the build never waits for
/// one; the trade is that the newline counting is part of the
/// worker's scan (it shares the pass with the match scan and never
/// runs on the UI thread).
pub struct FilterBuilder {
    /// Lines found so far, in document order.
    lines: Vec<FilterLine>,
    total_length: u64,
    /// Match-scan frontier: matches starting in `[0, scanned)`
    /// have been folded in.
    scanned: u64,
    /// Newline-counting frontier: newlines in `[0, count_pos)`
    /// have been counted into `count_lines`.
    count_pos: u64,
    /// Newlines in `[0, count_pos)` — the number of the line the
    /// counting frontier sits in.
    count_lines: u64,
    /// Start offset of the line the counting frontier sits in.
    line_start: u64,
}

impl FilterBuilder {
    pub fn new(total_length: u64) -> Self {
        Self {
            lines: Vec::new(),
            total_length,
            scanned: 0,
            count_pos: 0,
            count_lines: 0,
            line_start: 0,
        }
    }

    /// Bytes of the document the match scan has covered so far —
    /// the progress numerator.
    pub fn scanned(&self) -> u64 {
        self.scanned
    }

    pub fn total_length(&self) -> u64 {
        self.total_length
    }

    /// True once the scan has covered the whole document.
    pub fn is_done(&self) -> bool {
        self.scanned >= self.total_length
    }

    /// Count newlines forward until the counting frontier reaches
    /// `target`, keeping `line_start` on the start of the line the
    /// frontier ends in.
    fn count_to(&mut self, table: &PieceTable, target: u64) {
        let mut buffer = vec![0u8; 65536];
        while self.count_pos < target {
            let want = ((target - self.count_pos).min(buffer.len() as u64)) as usize;
            let read = table.read_into(self.count_pos, &mut buffer[..want]);
            if read == 0 {
                break;
            }
            let chunk = &buffer[..read];
            let newlines = crate::line_index::count_newlines_in(chunk);
            if newlines > 0 {
                let last = chunk
                    .iter()
                    .rposition(|&b| b == b'\n')
                    .expect("counted newlines are present");
                self.line_start = self.count_pos + last as u64 + 1;
                self.count_lines += newlines;
            }
            self.count_pos += read as u64;
        }
    }

    /// Advance the build to `window_end` (clamped to the document
    /// length): find the matches starting in the new window and
    /// fold their lines in. Returns how many NEW lines this window
    /// added. A `window_end` at or behind the current frontier is
    /// a no-op.
    pub fn advance(
        &mut self,
        table: &PieceTable,
        window_end: u64,
        needle: &[u8],
        options: &SearchOptions,
    ) -> usize {
        let window_end = window_end.min(self.total_length);
        if window_end <= self.scanned {
            return 0;
        }
        let matches = search::find_in_window(table, needle, options, self.scanned, window_end);
        let added_before = self.lines.len();
        for m in matches {
            self.count_to(table, m);
            let line_no = self.count_lines;
            if self
                .lines
                .last()
                .is_some_and(|l: &FilterLine| l.line_no == line_no)
            {
                continue;
            }
            let start = self.line_start;
            let end = line_content_end(table, start);
            self.lines.push(FilterLine {
                line_no,
                start,
                end,
            });
        }
        self.scanned = window_end;
        self.lines.len() - added_before
    }

    /// The view built so far. While the build runs
    /// (`complete == false`) `indexed_bytes` reports the covered
    /// prefix, exactly like a partial-index view; the final
    /// snapshot passes `complete == true`.
    pub fn snapshot(&self, complete: bool) -> FilterView {
        FilterView {
            lines: self.lines.clone(),
            complete,
            indexed_bytes: self.scanned,
            total_length: self.total_length,
        }
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

    // (Item 13) The incremental builder, driven window by window,
    // lands on exactly the view the one-shot build produces —
    // including with windows so small the needle straddles them.
    #[test]
    fn incremental_builder_matches_the_one_shot_build() {
        let data = log();
        let table = PieceTable::from_original(data.clone());
        let expected = view_for(
            &data,
            FilterQuery::literal(b"ERROR", SearchOptions::default()),
        );
        for window in [1u64, 3, 7, 64, 4096] {
            let mut builder = FilterBuilder::new(data.len() as u64);
            let mut end = 0u64;
            while !builder.is_done() {
                end += window;
                builder.advance(&table, end, b"ERROR", &SearchOptions::default());
            }
            let view = builder.snapshot(true);
            assert_eq!(view, expected, "window {window}");
            assert!(view.is_complete());
            assert_eq!(view.indexed_bytes(), data.len() as u64);
        }
        // A mid-build snapshot is honest about its coverage.
        let mut builder = FilterBuilder::new(data.len() as u64);
        builder.advance(&table, 20, b"ERROR", &SearchOptions::default());
        let partial = builder.snapshot(false);
        assert!(!partial.is_complete());
        assert_eq!(partial.indexed_bytes(), 20);
        // No-match and empty-needle builds complete empty.
        let mut builder = FilterBuilder::new(data.len() as u64);
        builder.advance(&table, u64::MAX, b"ZZZ", &SearchOptions::default());
        assert!(builder.is_done());
        assert!(builder.snapshot(true).is_empty());
        let mut builder = FilterBuilder::new(data.len() as u64);
        builder.advance(&table, u64::MAX, b"", &SearchOptions::default());
        assert!(builder.snapshot(true).is_empty());
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
