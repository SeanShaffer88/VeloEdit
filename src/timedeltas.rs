//! Time-delta core (roadmap R4): read the timestamp at the start
//! of a log line and compute how much time passed between
//! consecutive lines — the data behind a time-delta gutter that
//! makes stalls and bursts in a log visible at a glance.
//!
//! Accepted forms (the timestamp must start at the line's first
//! byte):
//!
//! - `YYYY-MM-DDTHH:MM:SS` — ISO 8601 with the `T` separator
//! - `YYYY-MM-DD HH:MM:SS` — the space-separated variant
//! - `HH:MM:SS` — time of day only
//!
//! Each may carry a fractional part `.f`, `.ff` or `.fff` (1–3
//! digits, right-padded to milliseconds). Anything else — wrong
//! field widths, hour 24, minute/second 60 (leap seconds are
//! rejected), February 30, a digit immediately after the timestamp
//! — does not parse.
//!
//! Results are milliseconds: since the Unix epoch for the dated
//! forms (reusing the civil-date conversion in
//! [`crate::inspector`]), since midnight for the time-only form.
//! Deltas between the two families are arithmetically consistent
//! only within a family — the gutter compares consecutive lines of
//! one log, which uses one format throughout.

use crate::inspector::{civil_from_days, days_from_civil};

/// Parse the timestamp at the start of `line` (see the module docs
/// for the accepted forms) into milliseconds.
pub fn parse_line_timestamp(line: &[u8]) -> Option<i64> {
    // Dated forms: "YYYY-MM-DD" (10 bytes) + separator + time.
    if line.len() >= 19
        && line.get(4) == Some(&b'-')
        && line.get(7) == Some(&b'-')
        && matches!(line.get(10), Some(b'T') | Some(b' '))
    {
        let year = field(line, 0, 4)?;
        let month = field(line, 5, 2)?;
        let day = field(line, 8, 2)?;
        let (hms, _) = parse_time(&line[11..])?;
        // Validate the date by round-tripping the civil
        // conversion: days_from_civil normalises (Feb 30 → Mar 2),
        // so only a true date comes back unchanged. The fields are
        // two digits each, so the u32 casts cannot lose anything.
        let (month32, day32) = (month as u32, day as u32);
        let days = days_from_civil(year, month32, day32);
        if civil_from_days(days) != (year, month32, day32) {
            return None;
        }
        return Some(days * 86_400_000 + hms);
    }
    // Time-only form.
    let (hms, _) = parse_time(line)?;
    Some(hms)
}

/// Read a fixed-width decimal field; `None` on a non-digit or a
/// short slice.
fn field(line: &[u8], start: usize, width: usize) -> Option<i64> {
    let slice = line.get(start..start + width)?;
    let mut value: i64 = 0;
    for &b in slice {
        if !b.is_ascii_digit() {
            return None;
        }
        value = value * 10 + i64::from(b - b'0');
    }
    Some(value)
}

/// Parse `HH:MM:SS[.fff]` at the start of `bytes`; returns the
/// milliseconds since midnight and the bytes consumed. The byte
/// after the timestamp must not be a digit (so `01:23:456` and a
/// 4-digit fraction do not parse).
fn parse_time(bytes: &[u8]) -> Option<(i64, usize)> {
    if bytes.len() < 8 || bytes.get(2) != Some(&b':') || bytes.get(5) != Some(&b':') {
        return None;
    }
    let hh = field(bytes, 0, 2)?;
    let mm = field(bytes, 3, 2)?;
    let ss = field(bytes, 6, 2)?;
    if hh >= 24 || mm >= 60 || ss >= 60 {
        return None;
    }
    let mut ms = (hh * 3600 + mm * 60 + ss) * 1000;
    let mut consumed = 8usize;
    if bytes.get(8) == Some(&b'.') {
        // 1–3 fraction digits, right-padded to milliseconds.
        let mut digits = 0usize;
        let mut frac: i64 = 0;
        while let Some(&b) = bytes.get(9 + digits) {
            if !b.is_ascii_digit() || digits == 3 {
                break;
            }
            frac = frac * 10 + i64::from(b - b'0');
            digits += 1;
        }
        if digits == 0 {
            return None;
        }
        for _ in digits..3 {
            frac *= 10;
        }
        ms += frac;
        consumed += 1 + digits;
    }
    if bytes.get(consumed).is_some_and(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((ms, consumed))
}

/// Per-line deltas for a sequence of lines: element *i* is
/// `ts[i] - ts[i-1]` in milliseconds when both lines carry a
/// timestamp, else `None`. The first line's delta is always
/// `None`, and an unparseable line yields `None` *and* breaks the
/// chain (the next line's delta is `None` too, since its
/// predecessor has no timestamp) — consecutive-pair semantics,
/// documented here because the alternative (delta since the last
/// parseable line) would silently span gaps.
pub fn line_time_deltas<'a>(lines: impl Iterator<Item = &'a [u8]>) -> Vec<Option<i64>> {
    let mut out = Vec::new();
    let mut previous: Option<i64> = None;
    for line in lines {
        let ts = parse_line_timestamp(line);
        out.push(match (previous, ts) {
            (Some(prev), Some(now)) => Some(now - prev),
            _ => None,
        });
        previous = ts;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dated_forms_parse_to_epoch_milliseconds() {
        // 2026-11-01T01:23:45.678 UTC. Days since epoch for
        // 2026-11-01 = 20,758 (pinned in inspector's tests);
        // 01:23:45.678 = 5,025,678 ms.
        let expected = 20_758 * 86_400_000 + 5_025_678;
        assert_eq!(
            parse_line_timestamp(b"2026-11-01T01:23:45.678 INFO started"),
            Some(expected)
        );
        assert_eq!(
            parse_line_timestamp(b"2026-11-01 01:23:45.678 INFO started"),
            Some(expected)
        );
        // No fraction, and 1–2 digit fractions right-pad.
        assert_eq!(
            parse_line_timestamp(b"2026-11-01T01:23:45"),
            Some(20_758 * 86_400_000 + 5_025_000)
        );
        assert_eq!(
            parse_line_timestamp(b"2026-11-01T01:23:45.6"),
            Some(20_758 * 86_400_000 + 5_025_600)
        );
        assert_eq!(
            parse_line_timestamp(b"2026-11-01T01:23:45.67"),
            Some(20_758 * 86_400_000 + 5_025_670)
        );
        // The epoch itself.
        assert_eq!(parse_line_timestamp(b"1970-01-01T00:00:00.000"), Some(0));
        // A leap day that exists parses.
        assert!(parse_line_timestamp(b"2024-02-29T12:00:00").is_some());
    }

    #[test]
    fn time_only_form_is_milliseconds_since_midnight() {
        assert_eq!(parse_line_timestamp(b"01:23:45.678 rest"), Some(5_025_678));
        assert_eq!(parse_line_timestamp(b"00:00:00"), Some(0));
        assert_eq!(parse_line_timestamp(b"23:59:59.999"), Some(86_399_999));
    }

    #[test]
    fn invalid_timestamps_do_not_parse() {
        for line in [
            &b"2026-02-30T01:00:00"[..],      // February 30 does not exist
            &b"2026-13-01T01:00:00"[..],      // month 13
            &b"2026-11-01T24:00:00"[..],      // hour 24
            &b"2026-11-01T01:60:00"[..],      // minute 60
            &b"2026-11-01T01:00:60"[..],      // leap second rejected
            &b"2026-11-01T01:00:00.6789"[..], // 4 fraction digits
            &b"2026-11-01T01:00:000"[..],     // digit run-on after seconds
            &b"2026-1-01T01:00:00"[..],       // wrong field width
            &b" 2026-11-01T01:00:00"[..],     // must start at byte 0
            &b"01:23:45."[..],                // fraction dot with no digits
            &b"not a timestamp"[..],
            &b""[..],
        ] {
            assert_eq!(parse_line_timestamp(line), None, "line: {line:?}");
        }
    }

    #[test]
    fn deltas_are_consecutive_pair_differences() {
        let lines: Vec<&[u8]> = vec![
            b"2026-11-01T01:00:00.000 first",
            b"2026-11-01T01:00:01.500 second", // +1500
            b"no timestamp here",              // None, breaks the chain
            b"2026-11-01T01:00:02.000 fourth", // None (predecessor unparseable)
            b"2026-11-01T01:00:02.250 fifth",  // +250
            b"2026-11-01T00:59:00.000 back",   // -62_250 (clock step back)
        ];
        assert_eq!(
            line_time_deltas(lines.into_iter()),
            vec![None, Some(1500), None, None, Some(250), Some(-62_250)]
        );
        assert!(line_time_deltas(std::iter::empty()).is_empty());
    }
}
