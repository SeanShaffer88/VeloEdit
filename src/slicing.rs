//! Multi-range slicing core (roadmap R4, extending item 44): carve
//! several non-contiguous byte or line ranges out of a document
//! into one output stream, in order, streamed in bounded chunks —
//! the whole document is never held in memory.
//!
//! # The range-spec grammar
//!
//! ```text
//! spec    := segment (';' segment)*
//! segment := byte-range | line-range
//! byte-range := number ['-' number]        ; byte offsets, BOTH ends inclusive
//! line-range := 'L' number ['-' 'L' number] ; 1-based line numbers, BOTH ends inclusive
//! number  := decimal digits | '0x' hex digits   ; ',' group separators allowed and ignored
//! ```
//!
//! Whitespace around segments and around `-` is ignored. A lone
//! number is a one-byte (or one-line) range. Examples:
//! `"1,000,000-1,050,000; 84,000,000-84,020,000"` (bytes),
//! `"L100-L200; L84,000,000"` (lines). Byte and line segments may
//! be mixed in one spec; every segment is resolved to bytes before
//! the ordering rules are checked.
//!
//! # Rules, all enforced with explicit errors
//!
//! - A range whose end is before its start is **reversed**.
//! - A byte range must lie inside the source; a line range must
//!   lie inside the document's line count (line ranges need a
//!   **complete** line index — a partial one cannot prove a line
//!   exists, so it is refused, never guessed).
//! - Segments must be in ascending order and must not overlap;
//!   adjacent segments (one starts where the previous ended) are
//!   fine.
//!
//! A line range exports whole lines *including* their line-ending
//! bytes: from the first line's first byte to the last line's last
//! byte (the final line of a file may have no ending).

use std::io::{self, Write};

use crate::line_index::LineIndex;
use crate::piece_table::PieceTable;
use crate::save;

/// One parsed segment of a range spec (see the module docs).
/// `end` is inclusive in both forms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeSpec {
    /// Byte offsets `[start, end]`, both inclusive.
    Bytes { start: u64, end: u64 },
    /// 1-based line numbers `[start, end]`, both inclusive.
    Lines { start: u64, end: u64 },
}

/// Why a slicing operation refused. Every refusal happens before
/// anything is written.
#[derive(Debug)]
pub enum SliceError {
    /// The spec string was blank.
    EmptySpec,
    /// A segment did not match the grammar.
    BadSegment(String),
    /// A number did not parse (or overflowed u64).
    BadNumber(String),
    /// A range's end is before its start.
    Reversed { start: u64, end: u64 },
    /// A byte range or line range reaches past the end of the
    /// source (carries what was asked and what exists).
    OutOfRange { asked: String, limit: String },
    /// A segment starts before an earlier segment (segments must
    /// be in ascending order).
    OutOfOrder { start: u64, previous_start: u64 },
    /// A segment overlaps an earlier segment.
    Overlap { start: u64, previous_end: u64 },
    /// The spec has line segments but no line index was supplied.
    IndexRequired,
    /// The supplied line index is a partial (staged) build.
    IncompleteIndex,
    /// The supplied line index describes a different length than
    /// the source.
    IndexMismatch { index_len: u64, source_len: u64 },
    /// Writing the output failed.
    Io(io::Error),
}

impl std::fmt::Display for SliceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SliceError::EmptySpec => write!(f, "the range spec is empty"),
            SliceError::BadSegment(s) => {
                write!(f, "range segment '{s}' does not match the grammar")
            }
            SliceError::BadNumber(s) => write!(f, "'{s}' is not a valid number"),
            SliceError::Reversed { start, end } => {
                write!(f, "range {start}-{end} is reversed (end before start)")
            }
            SliceError::OutOfRange { asked, limit } => {
                write!(f, "range {asked} reaches past the end ({limit})")
            }
            SliceError::OutOfOrder {
                start,
                previous_start,
            } => write!(
                f,
                "segment starting at {start} comes after a segment starting at {previous_start}; segments must be in ascending order"
            ),
            SliceError::Overlap {
                start,
                previous_end,
            } => write!(
                f,
                "segment starting at {start} overlaps the previous segment (which ends at {previous_end})"
            ),
            SliceError::IndexRequired => {
                write!(f, "the spec has line ranges but no line index was supplied")
            }
            SliceError::IncompleteIndex => write!(
                f,
                "the line index is still being built; line ranges need a complete index"
            ),
            SliceError::IndexMismatch {
                index_len,
                source_len,
            } => write!(
                f,
                "the line index describes {index_len} bytes but the source has {source_len}"
            ),
            SliceError::Io(e) => write!(f, "writing the extract failed: {e}"),
        }
    }
}

impl std::error::Error for SliceError {}

impl From<io::Error> for SliceError {
    fn from(e: io::Error) -> Self {
        SliceError::Io(e)
    }
}

/// Parse one number: decimal, or hex with a `0x`/`0X` prefix, with
/// `,` group separators ignored (`"1,000,000"`, `"0x1F,000"`).
fn parse_number(text: &str) -> Result<u64, SliceError> {
    let cleaned: String = text.chars().filter(|&c| c != ',').collect();
    if cleaned.is_empty() {
        return Err(SliceError::BadNumber(text.to_string()));
    }
    let value = if let Some(hex) = cleaned
        .strip_prefix("0x")
        .or_else(|| cleaned.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16)
    } else {
        cleaned.parse::<u64>()
    };
    value.map_err(|_| SliceError::BadNumber(text.to_string()))
}

/// Parse a range spec (see the module docs for the grammar).
pub fn parse_range_spec(spec: &str) -> Result<Vec<RangeSpec>, SliceError> {
    if spec.trim().is_empty() {
        return Err(SliceError::EmptySpec);
    }
    let mut out = Vec::new();
    for raw in spec.split(';') {
        let segment = raw.trim();
        if segment.is_empty() {
            return Err(SliceError::BadSegment(raw.to_string()));
        }
        out.push(parse_segment(segment)?);
    }
    Ok(out)
}

fn parse_segment(segment: &str) -> Result<RangeSpec, SliceError> {
    let is_line = segment.starts_with('L') || segment.starts_with('l');
    let mut parts = segment.splitn(2, '-');
    let first = parts.next().unwrap_or("").trim();
    let second = parts.next().map(|p| p.trim());
    let parse_side = |text: &str| -> Result<u64, SliceError> {
        let digits = if is_line {
            text.strip_prefix(['L', 'l'])
                .ok_or_else(|| SliceError::BadSegment(segment.to_string()))?
        } else {
            if text.starts_with(['L', 'l']) {
                return Err(SliceError::BadSegment(segment.to_string()));
            }
            text
        };
        parse_number(digits.trim())
    };
    let start = parse_side(first)?;
    let end = match second {
        Some(text) => parse_side(text)?,
        None => start,
    };
    Ok(if is_line {
        RangeSpec::Lines { start, end }
    } else {
        RangeSpec::Bytes { start, end }
    })
}

/// Resolve parsed segments to half-open byte ranges `[start, end)`
/// in spec order, enforcing every rule in the module docs. Line
/// segments resolve through `index` with `read_at` (the
/// [`LineIndex`] read contract); byte-only specs never touch the
/// index, and `index` may be `None` for them.
pub fn resolve_ranges(
    specs: &[RangeSpec],
    source_len: u64,
    index: Option<&LineIndex>,
    mut read_at: impl FnMut(u64, &mut [u8]) -> usize,
) -> Result<Vec<(u64, u64)>, SliceError> {
    let mut resolved: Vec<(u64, u64)> = Vec::with_capacity(specs.len());
    for spec in specs {
        let range = match *spec {
            RangeSpec::Bytes { start, end } => {
                if end < start {
                    return Err(SliceError::Reversed { start, end });
                }
                if end >= source_len {
                    return Err(SliceError::OutOfRange {
                        asked: format!("byte {end}"),
                        limit: format!("the source is {source_len} bytes"),
                    });
                }
                (start, end + 1)
            }
            RangeSpec::Lines { start, end } => {
                if end < start {
                    return Err(SliceError::Reversed { start, end });
                }
                let index = index.ok_or(SliceError::IndexRequired)?;
                if !index.is_complete() {
                    return Err(SliceError::IncompleteIndex);
                }
                if index.total_length() != source_len {
                    return Err(SliceError::IndexMismatch {
                        index_len: index.total_length(),
                        source_len,
                    });
                }
                if start == 0 {
                    return Err(SliceError::OutOfRange {
                        asked: "line 0".to_string(),
                        limit: "line numbers start at 1".to_string(),
                    });
                }
                let line_count = index.line_count();
                if end > line_count {
                    return Err(SliceError::OutOfRange {
                        asked: format!("line {end}"),
                        limit: format!("the document has {line_count} lines"),
                    });
                }
                let start_byte = index
                    .line_start_offset(start - 1, &mut read_at)
                    .ok_or_else(|| SliceError::OutOfRange {
                        asked: format!("line {start}"),
                        limit: format!("the document has {line_count} lines"),
                    })?;
                let end_byte = if end == line_count {
                    source_len
                } else {
                    index.line_start_offset(end, &mut read_at).ok_or_else(|| {
                        SliceError::OutOfRange {
                            asked: format!("line {end}"),
                            limit: format!("the document has {line_count} lines"),
                        }
                    })?
                };
                (start_byte, end_byte)
            }
        };
        if let Some(&(prev_start, prev_end)) = resolved.last() {
            if range.0 < prev_start {
                return Err(SliceError::OutOfOrder {
                    start: range.0,
                    previous_start: prev_start,
                });
            }
            if range.0 < prev_end {
                return Err(SliceError::Overlap {
                    start: range.0,
                    previous_end: prev_end,
                });
            }
        }
        resolved.push(range);
    }
    Ok(resolved)
}

/// What an [`extract_ranges`] run wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtractReport {
    /// How many ranges were written.
    pub ranges: usize,
    /// Total bytes written.
    pub bytes_written: u64,
}

/// Parse `spec`, resolve it against `table` (and `index` for line
/// segments), and stream the ranges to `out` in order. Every
/// segment is written with the save pipeline's range writer
/// (bounded chunks), so memory stays flat however large the ranges
/// are. All validation happens before the first byte is written.
pub fn extract_ranges(
    table: &PieceTable,
    index: Option<&LineIndex>,
    spec: &str,
    out: &mut impl Write,
) -> Result<ExtractReport, SliceError> {
    let specs = parse_range_spec(spec)?;
    let ranges = resolve_ranges(&specs, table.len(), index, |offset, buf| {
        table.read_into(offset, buf)
    })?;
    let mut bytes_written = 0u64;
    for &(start, end) in &ranges {
        bytes_written += save::write_table_range(table, start, end - start, out)?;
    }
    Ok(ExtractReport {
        ranges: ranges.len(),
        bytes_written,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Vec<u8> {
        // 5 lines of exactly 10 bytes each: "line-0000\n" .. "line-0004\n".
        let mut data = Vec::new();
        for i in 0..5 {
            data.extend_from_slice(format!("line-{i:04}\n").as_bytes());
        }
        data
    }

    fn reader_for(data: &[u8]) -> impl FnMut(u64, &mut [u8]) -> usize + '_ {
        move |offset: u64, out: &mut [u8]| {
            let s = offset as usize;
            if s >= data.len() {
                return 0;
            }
            let n = out.len().min(data.len() - s);
            out[..n].copy_from_slice(&data[s..s + n]);
            n
        }
    }

    #[test]
    fn grammar_parses_byte_line_hex_and_grouped_forms() {
        assert_eq!(
            parse_range_spec("1,000,000-1,050,000; 84,000,000-84,020,000").unwrap(),
            vec![
                RangeSpec::Bytes {
                    start: 1_000_000,
                    end: 1_050_000
                },
                RangeSpec::Bytes {
                    start: 84_000_000,
                    end: 84_020_000
                },
            ]
        );
        assert_eq!(
            parse_range_spec("L100-L200").unwrap(),
            vec![RangeSpec::Lines {
                start: 100,
                end: 200
            }]
        );
        assert_eq!(
            parse_range_spec("0x10-0x1F").unwrap(),
            vec![RangeSpec::Bytes { start: 16, end: 31 }]
        );
        assert_eq!(
            parse_range_spec(" 42 ").unwrap(),
            vec![RangeSpec::Bytes { start: 42, end: 42 }]
        );
        assert_eq!(
            parse_range_spec("L7").unwrap(),
            vec![RangeSpec::Lines { start: 7, end: 7 }]
        );
        assert_eq!(
            parse_range_spec("L3 - L4 ; 100 - 200").unwrap(),
            vec![
                RangeSpec::Lines { start: 3, end: 4 },
                RangeSpec::Bytes {
                    start: 100,
                    end: 200
                },
            ]
        );
    }

    #[test]
    fn grammar_rejects_bad_input_explicitly() {
        assert!(matches!(parse_range_spec(""), Err(SliceError::EmptySpec)));
        assert!(matches!(
            parse_range_spec("   "),
            Err(SliceError::EmptySpec)
        ));
        assert!(matches!(
            parse_range_spec("10-"),
            Err(SliceError::BadNumber(_))
        ));
        assert!(matches!(
            parse_range_spec("abc"),
            Err(SliceError::BadNumber(_))
        ));
        assert!(matches!(
            parse_range_spec("10;;20"),
            Err(SliceError::BadSegment(_))
        ));
        // A line segment's second side must carry its own L.
        assert!(matches!(
            parse_range_spec("L10-20"),
            Err(SliceError::BadSegment(_))
        ));
        // A byte segment may not grow an L halfway through.
        assert!(matches!(
            parse_range_spec("10-L20"),
            Err(SliceError::BadSegment(_))
        ));
        assert!(matches!(
            parse_range_spec("99999999999999999999999999"),
            Err(SliceError::BadNumber(_))
        ));
    }

    #[test]
    fn resolve_enforces_reversed_out_of_range_overlap_and_order() {
        let data = doc(); // 50 bytes, 5 lines
        let index = LineIndex::build_from_bytes(&data);
        let mut reader = reader_for(&data);
        // Reversed.
        assert!(matches!(
            resolve_ranges(
                &[RangeSpec::Bytes { start: 9, end: 4 }],
                50,
                None,
                &mut reader
            ),
            Err(SliceError::Reversed { .. })
        ));
        // Byte past the end (49 is the last valid inclusive end).
        assert!(matches!(
            resolve_ranges(
                &[RangeSpec::Bytes { start: 0, end: 50 }],
                50,
                None,
                &mut reader
            ),
            Err(SliceError::OutOfRange { .. })
        ));
        // Line past the end.
        assert!(matches!(
            resolve_ranges(
                &[RangeSpec::Lines { start: 1, end: 6 }],
                50,
                Some(&index),
                &mut reader
            ),
            Err(SliceError::OutOfRange { .. })
        ));
        // Line 0 does not exist (1-based).
        assert!(matches!(
            resolve_ranges(
                &[RangeSpec::Lines { start: 0, end: 1 }],
                50,
                Some(&index),
                &mut reader
            ),
            Err(SliceError::OutOfRange { .. })
        ));
        // Overlap and disorder.
        let overlapping = parse_range_spec("0-19; 10-29").unwrap();
        assert!(matches!(
            resolve_ranges(&overlapping, 50, None, &mut reader),
            Err(SliceError::Overlap { .. })
        ));
        let disordered = parse_range_spec("20-29; 0-9").unwrap();
        assert!(matches!(
            resolve_ranges(&disordered, 50, None, &mut reader),
            Err(SliceError::OutOfOrder { .. })
        ));
        // Adjacent is fine and merges conceptually into two ranges.
        let adjacent = parse_range_spec("0-19; 20-29").unwrap();
        assert_eq!(
            resolve_ranges(&adjacent, 50, None, &mut reader).unwrap(),
            vec![(0, 20), (20, 30)]
        );
        // Line segments without an index, and with a partial one.
        assert!(matches!(
            resolve_ranges(
                &[RangeSpec::Lines { start: 1, end: 2 }],
                50,
                None,
                &mut reader
            ),
            Err(SliceError::IndexRequired)
        ));
        let partial = LineIndex::build_prefix(50, 0, reader_for(&data));
        assert!(matches!(
            resolve_ranges(
                &[RangeSpec::Lines { start: 1, end: 2 }],
                50,
                Some(&partial),
                &mut reader
            ),
            Err(SliceError::IncompleteIndex)
        ));
    }

    #[test]
    fn extract_streams_the_selected_ranges_in_order() {
        let data = doc();
        let table = PieceTable::from_original(data.clone());
        let index = LineIndex::build_from_bytes(&data);
        let mut out: Vec<u8> = Vec::new();
        // Bytes 0..=3 ("line"), then lines 2 and 4 (1-based) —
        // ascending, as the ordering rule requires.
        let report = extract_ranges(&table, Some(&index), "0-3; L2; L4", &mut out).unwrap();
        assert_eq!(report.ranges, 3);
        assert_eq!(out, b"lineline-0001\nline-0003\n");
        assert_eq!(report.bytes_written, out.len() as u64);
    }

    #[test]
    fn line_ranges_include_endings_and_reach_the_file_end() {
        let data = doc();
        let table = PieceTable::from_original(data.clone());
        let index = LineIndex::build_from_bytes(&data);
        let mut out: Vec<u8> = Vec::new();
        extract_ranges(&table, Some(&index), "L5", &mut out).unwrap();
        assert_eq!(out, b"line-0004\n");
        // A final line without a trailing newline exports whole.
        let data2 = b"a\nb\nc".to_vec();
        let table2 = PieceTable::from_original(data2.clone());
        let index2 = LineIndex::build_from_bytes(&data2);
        let mut out2: Vec<u8> = Vec::new();
        extract_ranges(&table2, Some(&index2), "L2-L3", &mut out2).unwrap();
        assert_eq!(out2, b"b\nc");
    }
}
