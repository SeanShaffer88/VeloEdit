//! Strings core (roadmap R4): scan a byte source for printable
//! text runs — the data behind a strings pane that lists what text
//! hides inside a binary file, each with its exact offset for a
//! one-click jump.
//!
//! Two run kinds are recognised:
//!
//! - **ASCII runs**: bytes in `0x20..=0x7E` (printable ASCII,
//!   space included, DEL and control bytes excluded).
//! - **UTF-16LE runs**: pairs of (printable ASCII byte, `0x00`) —
//!   how ASCII text appears inside UTF-16LE data.
//!
//! `min_len` counts **characters** in both kinds (default
//! [`DEFAULT_MIN_LEN`] = 4): a UTF-16LE hit of `min_len` characters
//! is `2 * min_len` bytes long. An ASCII run that falls strictly
//! inside a UTF-16LE run's byte range (its first characters seen
//! without their zero partners) is folded away — the UTF-16LE hit
//! is the real one.
//!
//! The scan is windowed (64 KiB windows with run state carried
//! across boundaries), so memory stays flat on any file size. Each
//! hit's preview is capped at [`PREVIEW_CHARS`] characters.

use crate::piece_table::PieceTable;

/// Default minimum run length, in characters.
pub const DEFAULT_MIN_LEN: usize = 4;

/// How many characters of a run the preview keeps.
pub const PREVIEW_CHARS: usize = 64;

/// Scan window size.
const WINDOW: usize = 64 * 1024;

/// One text run found in the source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundString {
    /// Byte offset of the run's first byte.
    pub offset: u64,
    /// The run's length in bytes (2× the character count for
    /// UTF-16LE runs).
    pub byte_len: u64,
    /// The decoded text, capped at [`PREVIEW_CHARS`] characters.
    pub text: String,
}

fn is_printable(b: u8) -> bool {
    (0x20..=0x7E).contains(&b)
}

/// Finish a run: keep it when it meets the minimum length, with
/// its preview capped.
fn finish_run(
    out: &mut Vec<FoundString>,
    start: u64,
    chars: &[u8],
    bytes_per_char: u64,
    min_len: usize,
) {
    if chars.len() >= min_len && !chars.is_empty() {
        let preview: String = chars
            .iter()
            .take(PREVIEW_CHARS)
            .map(|&b| b as char)
            .collect();
        out.push(FoundString {
            offset: start,
            byte_len: chars.len() as u64 * bytes_per_char,
            text: preview,
        });
    }
}

/// Scan `total` bytes from a byte source for ASCII and UTF-16LE
/// text runs of at least `min_len` characters. `read_at` follows
/// the usual read contract; a short or empty read ends the scan
/// (what was found stands). Results are in ascending offset order.
pub fn scan_strings(
    total: u64,
    mut read_at: impl FnMut(u64, &mut [u8]) -> usize,
    min_len: usize,
) -> Vec<FoundString> {
    let min_len = min_len.max(1);
    let mut ascii_hits: Vec<FoundString> = Vec::new();
    let mut utf16_hits: Vec<FoundString> = Vec::new();

    // Run state carried across windows. ASCII: the current run's
    // characters and start. UTF-16LE: the same, plus a pending
    // high byte waiting for its zero partner (so a pair split by a
    // window edge still joins).
    let mut ascii_run: Vec<u8> = Vec::new();
    let mut ascii_start: u64 = 0;
    let mut u16_run: Vec<u8> = Vec::new();
    let mut u16_start: u64 = 0;
    let mut u16_pending: Option<(u8, u64)> = None; // (byte, its offset)

    let mut window = vec![0u8; WINDOW];
    let mut offset: u64 = 0;
    while offset < total {
        let want = ((total - offset) as usize).min(window.len());
        let read = read_at(offset, &mut window[..want]);
        if read == 0 {
            break;
        }
        for (i, &b) in window[..read].iter().enumerate() {
            let at = offset + i as u64;
            // ASCII run tracking.
            if is_printable(b) {
                if ascii_run.is_empty() {
                    ascii_start = at;
                }
                ascii_run.push(b);
            } else if !ascii_run.is_empty() {
                finish_run(&mut ascii_hits, ascii_start, &ascii_run, 1, min_len);
                ascii_run.clear();
            }
            // UTF-16LE pair tracking.
            match u16_pending.take() {
                Some((high, high_at)) => {
                    if b == 0 {
                        if u16_run.is_empty() {
                            u16_start = high_at;
                        }
                        u16_run.push(high);
                    } else {
                        // Not a pair: the run (if any) ends at the
                        // pending byte, and this byte may itself
                        // start a new pair.
                        if !u16_run.is_empty() {
                            finish_run(&mut utf16_hits, u16_start, &u16_run, 2, min_len);
                            u16_run.clear();
                        }
                        if is_printable(b) {
                            u16_pending = Some((b, at));
                        }
                    }
                }
                None => {
                    if is_printable(b) {
                        u16_pending = Some((b, at));
                    } else if !u16_run.is_empty() {
                        finish_run(&mut utf16_hits, u16_start, &u16_run, 2, min_len);
                        u16_run.clear();
                    }
                }
            }
        }
        offset += read as u64;
    }
    if !ascii_run.is_empty() {
        finish_run(&mut ascii_hits, ascii_start, &ascii_run, 1, min_len);
    }
    if !u16_run.is_empty() {
        finish_run(&mut utf16_hits, u16_start, &u16_run, 2, min_len);
    }

    // Merge: UTF-16LE hits win over ASCII hits strictly inside
    // them; the rest interleave by offset.
    let mut out: Vec<FoundString> = Vec::with_capacity(ascii_hits.len() + utf16_hits.len());
    for hit in ascii_hits {
        let inside_u16 = utf16_hits
            .iter()
            .any(|u| hit.offset > u.offset && hit.offset + hit.byte_len < u.offset + u.byte_len);
        if !inside_u16 {
            out.push(hit);
        }
    }
    out.extend(utf16_hits);
    out.sort_by_key(|h| h.offset);
    out
}

/// Scan a document's piece table (the strings of the *edited*
/// content, not just the file on disk).
pub fn scan_strings_in_table(table: &PieceTable, min_len: usize) -> Vec<FoundString> {
    scan_strings(
        table.len(),
        |offset, out| table.read_into(offset, out),
        min_len,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader(data: &[u8]) -> impl FnMut(u64, &mut [u8]) -> usize + '_ {
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

    fn utf16le(text: &str) -> Vec<u8> {
        text.bytes().flat_map(|b| [b, 0]).collect()
    }

    #[test]
    fn ascii_runs_found_with_exact_offsets() {
        let mut data = vec![0x00, 0x01, 0xFF];
        data.extend_from_slice(b"Hello, World!");
        data.extend_from_slice(&[0x00, 0x7F]);
        data.extend_from_slice(b"abc"); // too short at default min 4
        data.push(0x00);
        data.extend_from_slice(b"second run here");
        let hits = scan_strings(data.len() as u64, reader(&data), DEFAULT_MIN_LEN);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].offset, 3);
        assert_eq!(hits[0].byte_len, 13);
        assert_eq!(hits[0].text, "Hello, World!");
        assert_eq!(hits[1].offset, 3 + 13 + 2 + 3 + 1);
        assert_eq!(hits[1].text, "second run here");
    }

    #[test]
    fn utf16le_runs_found_and_their_ascii_shadows_folded() {
        let mut data = vec![0xAA, 0xBB];
        data.extend_from_slice(&utf16le("Wide text!"));
        data.extend_from_slice(&[0x00, 0x00]);
        data.extend_from_slice(b"plain ascii run");
        let hits = scan_strings(data.len() as u64, reader(&data), DEFAULT_MIN_LEN);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].offset, 2);
        assert_eq!(hits[0].byte_len, 20);
        assert_eq!(hits[0].text, "Wide text!");
        assert_eq!(hits[1].text, "plain ascii run");
        assert_eq!(hits[1].byte_len, 15);
    }

    #[test]
    fn min_len_counts_characters_for_both_kinds() {
        let mut data = utf16le("abc"); // 3 chars / 6 bytes
        data.push(0x00);
        data.extend_from_slice(b"abcd");
        // At min 4 the UTF-16 run (3 chars) is out, the ASCII in.
        let hits = scan_strings(data.len() as u64, reader(&data), 4);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "abcd");
        // At min 3 both are in.
        let hits = scan_strings(data.len() as u64, reader(&data), 3);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].text, "abc");
        assert_eq!(hits[0].byte_len, 6);
    }

    #[test]
    fn runs_crossing_the_window_edge_are_whole() {
        // A run that starts just before the 64 KiB window edge and
        // continues past it must come back as one hit.
        let mut data = vec![0u8; WINDOW - 2];
        data.extend_from_slice(b"crossing the boundary run");
        data.extend_from_slice(&[0u8; 16]);
        // A UTF-16LE run whose pairs straddle the edge too.
        let mut data2 = vec![0u8; WINDOW - 3];
        data2.extend_from_slice(&utf16le("straddling pairs here"));
        let hits = scan_strings(data.len() as u64, reader(&data), DEFAULT_MIN_LEN);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "crossing the boundary run");
        assert_eq!(hits[0].offset, (WINDOW - 2) as u64);
        let hits = scan_strings(data2.len() as u64, reader(&data2), DEFAULT_MIN_LEN);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "straddling pairs here");
        assert_eq!(hits[0].offset, (WINDOW - 3) as u64);
        assert_eq!(hits[0].byte_len, 42);
    }

    #[test]
    fn preview_caps_and_table_scan_agrees() {
        let data = vec![b'x'; 100];
        let hits = scan_strings(data.len() as u64, reader(&data), DEFAULT_MIN_LEN);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].byte_len, 100);
        assert_eq!(hits[0].text.chars().count(), PREVIEW_CHARS);
        let table = PieceTable::from_original(data);
        let via_table = scan_strings_in_table(&table, DEFAULT_MIN_LEN);
        assert_eq!(via_table.len(), 1);
        assert_eq!(via_table[0].byte_len, 100);
    }
}
