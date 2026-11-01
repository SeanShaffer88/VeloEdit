//! Data inspector core (roadmap item 15): read the bytes at the
//! caret as numbers.
//!
//! [`inspect`] parses a byte slice from its first byte as every
//! scalar the inspector panel shows — signed and unsigned integers
//! of 8/16/32/64 bits in both byte orders, 32- and 64-bit floats in
//! both orders, a hex and binary rendering of the first eight
//! bytes, and Unix-timestamp readings of the unsigned values.
//! A reading that needs more bytes than the slice holds is `None`,
//! so a caret near the end of a file simply greys those rows out.
//!
//! Timestamps are formatted by hand (days-to-civil-date below) so
//! the module stays dependency-free. A reading outside the sane
//! range 1970-01-01 ..= 2099-12-31 is reported as `"—"` rather
//! than as an absurd date.
//!
//! The report also carries a GUID reading (the first three fields
//! read little-endian, the way Windows stores GUIDs in files), a
//! Windows FILETIME reading (100-nanosecond ticks since
//! 1601-01-01, sane range 1601 ..= 2099), and a short text preview
//! of the longest valid UTF-8 run at the start of the slice.

/// Seconds at 2100-01-01 00:00:00 UTC: the exclusive top of the
/// sane timestamp range.
const TIMESTAMP_MAX_SECONDS: u64 = 4_102_444_800;

/// Seconds between the FILETIME epoch (1601-01-01) and the Unix
/// epoch (1970-01-01).
const FILETIME_EPOCH_OFFSET_SECONDS: u64 = 11_644_473_600;

/// FILETIME ticks (100 ns) at 2100-01-01 00:00:00 UTC: the
/// exclusive top of the sane FILETIME range.
const FILETIME_MAX_TICKS: u64 =
    (TIMESTAMP_MAX_SECONDS + FILETIME_EPOCH_OFFSET_SECONDS) * 10_000_000;

/// Longest text preview the inspector reports (characters).
const PREVIEW_MAX_CHARS: usize = 32;

/// The "no sane reading" marker used by the timestamp fields.
const NO_READING: &str = "—";

/// Everything the inspector panel shows for one byte position.
#[derive(Clone, Debug, PartialEq)]
pub struct InspectorReport {
    /// How many bytes the slice actually held.
    pub len: usize,
    pub u8: Option<u8>,
    pub i8: Option<i8>,
    pub u16_le: Option<u16>,
    pub u16_be: Option<u16>,
    pub i16_le: Option<i16>,
    pub i16_be: Option<i16>,
    pub u32_le: Option<u32>,
    pub u32_be: Option<u32>,
    pub i32_le: Option<i32>,
    pub i32_be: Option<i32>,
    pub f32_le: Option<f32>,
    pub f32_be: Option<f32>,
    pub u64_le: Option<u64>,
    pub u64_be: Option<u64>,
    pub i64_le: Option<i64>,
    pub i64_be: Option<i64>,
    pub f64_le: Option<f64>,
    pub f64_be: Option<f64>,
    /// The first up-to-8 bytes as upper-case hex pairs, space
    /// separated (for example `"23 20 56 65"`).
    pub hex: String,
    /// The first up-to-8 bytes as 8-bit binary groups, space
    /// separated (for example `"00100011 00100000"`).
    pub binary: String,
    /// The little-endian u32 read as Unix seconds, formatted UTC,
    /// or `"—"` when absent or out of the sane range.
    pub unix_seconds_u32: String,
    /// The little-endian u64 read as Unix seconds, same format.
    pub unix_seconds_u64: String,
    /// The little-endian u64 read as Unix milliseconds, formatted
    /// with a `.mmm` fraction, or `"—"`.
    pub unix_millis_u64: String,
    /// The first 16 bytes as a GUID, in the standard
    /// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` layout: the first
    /// three fields are read little-endian (the storage order
    /// Windows uses), the last two are written as the raw bytes.
    /// `None` when the slice holds fewer than 16 bytes.
    pub guid: Option<String>,
    /// The little-endian u64 read as a Windows FILETIME (100-ns
    /// ticks since 1601-01-01), formatted UTC, or `"—"` when
    /// absent or outside the sane range (1601 up to 2100).
    pub filetime_u64: String,
    /// A short preview of the longest valid UTF-8 run starting at
    /// the first byte: control characters dropped, at most 32
    /// characters, or `"—"` when nothing printable remains.
    pub utf8_preview: String,
}

fn take<const N: usize>(bytes: &[u8]) -> Option<[u8; N]> {
    bytes.first_chunk::<N>().copied()
}

/// Parse `bytes` from its first byte into an [`InspectorReport`].
/// Integer and float readings use the bytes in order; multi-byte
/// readings are `None` when the slice is too short for them.
pub fn inspect(bytes: &[u8]) -> InspectorReport {
    let u16le = take::<2>(bytes).map(u16::from_le_bytes);
    let u16be = take::<2>(bytes).map(u16::from_be_bytes);
    let u32le = take::<4>(bytes).map(u32::from_le_bytes);
    let u32be = take::<4>(bytes).map(u32::from_be_bytes);
    let u64le = take::<8>(bytes).map(u64::from_le_bytes);
    let u64be = take::<8>(bytes).map(u64::from_be_bytes);
    let shown = &bytes[..bytes.len().min(8)];
    InspectorReport {
        len: bytes.len(),
        u8: bytes.first().copied(),
        i8: bytes.first().map(|b| *b as i8),
        u16_le: u16le,
        u16_be: u16be,
        i16_le: u16le.map(|v| v as i16),
        i16_be: u16be.map(|v| v as i16),
        u32_le: u32le,
        u32_be: u32be,
        i32_le: u32le.map(|v| v as i32),
        i32_be: u32be.map(|v| v as i32),
        f32_le: u32le.map(f32::from_bits),
        f32_be: u32be.map(f32::from_bits),
        u64_le: u64le,
        u64_be: u64be,
        i64_le: u64le.map(|v| v as i64),
        i64_be: u64be.map(|v| v as i64),
        f64_le: u64le.map(f64::from_bits),
        f64_be: u64be.map(f64::from_bits),
        hex: shown
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(" "),
        binary: shown
            .iter()
            .map(|b| format!("{b:08b}"))
            .collect::<Vec<_>>()
            .join(" "),
        unix_seconds_u32: u32le
            .map(|v| format_unix_seconds(v as u64))
            .unwrap_or_else(|| NO_READING.to_string()),
        unix_seconds_u64: u64le
            .map(format_unix_seconds)
            .unwrap_or_else(|| NO_READING.to_string()),
        unix_millis_u64: u64le
            .map(format_unix_millis)
            .unwrap_or_else(|| NO_READING.to_string()),
        guid: take::<16>(bytes).map(|b| format_guid(&b)),
        filetime_u64: u64le
            .map(format_filetime)
            .unwrap_or_else(|| NO_READING.to_string()),
        utf8_preview: utf8_preview(bytes),
    }
}

/// Format 16 bytes as a GUID string. Windows stores the first
/// three fields (a u32 and two u16s) little-endian, so they are
/// read that way here; the final eight bytes are written in
/// order, the last six without separators (the standard text
/// layout: 8-4-4-4-12 hex digits).
fn format_guid(bytes: &[u8; 16]) -> String {
    let d1 = u32::from_le_bytes(bytes[0..4].try_into().expect("4 bytes"));
    let d2 = u16::from_le_bytes(bytes[4..6].try_into().expect("2 bytes"));
    let d3 = u16::from_le_bytes(bytes[6..8].try_into().expect("2 bytes"));
    format!(
        "{d1:08X}-{d2:04X}-{d3:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

/// Format Windows FILETIME `ticks` (100-nanosecond intervals
/// since 1601-01-01) as `"YYYY-MM-DD HH:MM:SS UTC"`, or the
/// no-reading marker outside the sane range (1601-01-01 up to but
/// not including 2100-01-01). Sub-second ticks are dropped, like
/// the Unix-seconds readings.
pub fn format_filetime(ticks: u64) -> String {
    if ticks >= FILETIME_MAX_TICKS {
        return NO_READING.to_string();
    }
    let seconds_since_1601 = ticks / 10_000_000;
    let days = (seconds_since_1601 / 86_400) as i64 - 134_774; // days 1601→1970
    let day_secs = seconds_since_1601 % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        day_secs / 3600,
        (day_secs / 60) % 60,
        day_secs % 60
    )
}

/// The text preview for the inspector: the longest valid UTF-8
/// run starting at the first byte, with control characters
/// dropped, capped at [`PREVIEW_MAX_CHARS`] characters. The
/// no-reading marker when the run holds nothing printable (an
/// empty slice, an invalid first byte, or only control codes).
fn utf8_preview(bytes: &[u8]) -> String {
    let valid_len = match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(err) => err.valid_up_to(),
    };
    // valid_up_to is the longest valid prefix, so this never errs.
    let run = std::str::from_utf8(&bytes[..valid_len]).unwrap_or("");
    let preview: String = run
        .chars()
        .filter(|c| !c.is_control())
        .take(PREVIEW_MAX_CHARS)
        .collect();
    if preview.is_empty() {
        NO_READING.to_string()
    } else {
        preview
    }
}

/// Format Unix `seconds` as `"YYYY-MM-DD HH:MM:SS UTC"`, or the
/// no-reading marker when the value falls outside the sane range
/// (1970-01-01 up to but not including 2100-01-01).
pub fn format_unix_seconds(seconds: u64) -> String {
    if seconds >= TIMESTAMP_MAX_SECONDS {
        return NO_READING.to_string();
    }
    let days = seconds / 86_400;
    let day_secs = seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        day_secs / 3600,
        (day_secs / 60) % 60,
        day_secs % 60
    )
}

/// Format Unix `millis` (milliseconds since the epoch) like
/// [`format_unix_seconds`] with a `.mmm` fraction appended.
pub fn format_unix_millis(millis: u64) -> String {
    if millis >= TIMESTAMP_MAX_SECONDS * 1000 {
        return NO_READING.to_string();
    }
    let seconds = millis / 1000;
    let fraction = millis % 1000;
    let whole = format_unix_seconds(seconds);
    if whole == NO_READING {
        return whole;
    }
    // Replace the trailing " UTC" with the fraction plus " UTC".
    // `whole` came from `format_unix_seconds` and is not NO_READING
    // (checked above), so its format string guarantees the suffix;
    // the fallback is unreachable and returns the string unmodified
    // rather than panicking (architectural audit: dead error path
    // removed).
    let base = whole.strip_suffix(" UTC").unwrap_or(&whole);
    format!("{base}.{fraction:03} UTC")
}

/// Days since 1970-01-01 → (year, month, day), by Howard
/// Hinnant's well-known civil-date algorithm. The Unix readings
/// never pass a negative count, but the FILETIME reading reaches
/// back to 1601, and the era arithmetic below is exact for those
/// negative inputs too (down to −134,774 days).
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // day of era, [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // year of era, [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year, [0, 365]
    let mp = (5 * doy + 2) / 153; // month index, [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (
        if month <= 2 { year + 1 } else { year },
        month as u32,
        day as u32,
    )
}

/// (year, month, day) → days since 1970-01-01: the exact inverse
/// of [`civil_from_days`], by the same algorithm. Out-of-range
/// fields are *normalised*, not rejected (February 30 becomes
/// March 2), so a caller validating a parsed date must round-trip
/// through [`civil_from_days`] and compare — the time-delta core
/// (`crate::timedeltas`) does exactly that.
pub(crate) fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // year of era, [0, 399]
    let mp = i64::from(if month > 2 { month - 3 } else { month + 9 }); // [0, 11]
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1; // day of year
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // day of era
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_has_no_readings() {
        let report = inspect(&[]);
        assert_eq!(report.len, 0);
        assert_eq!(report.u8, None);
        assert_eq!(report.u64_le, None);
        assert_eq!(report.hex, "");
        assert_eq!(report.binary, "");
        assert_eq!(report.unix_seconds_u32, "—");
        assert_eq!(report.unix_seconds_u64, "—");
        assert_eq!(report.unix_millis_u64, "—");
        assert_eq!(report.guid, None);
        assert_eq!(report.filetime_u64, "—");
        assert_eq!(report.utf8_preview, "—");
    }

    #[test]
    fn scalar_readings_in_both_orders() {
        // 0x01 0x02 ... 0x08: every reading is a distinct value.
        let bytes: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
        let r = inspect(&bytes);
        assert_eq!(r.u8, Some(1));
        assert_eq!(r.i8, Some(1));
        assert_eq!(r.u16_le, Some(0x0201));
        assert_eq!(r.u16_be, Some(0x0102));
        assert_eq!(r.i16_le, Some(0x0201));
        assert_eq!(r.u32_le, Some(0x04030201));
        assert_eq!(r.u32_be, Some(0x01020304));
        assert_eq!(r.i32_be, Some(0x01020304));
        assert_eq!(r.u64_le, Some(0x0807060504030201));
        assert_eq!(r.u64_be, Some(0x0102030405060708));
        assert_eq!(r.i64_le, Some(0x0807060504030201));
        assert_eq!(r.hex, "01 02 03 04 05 06 07 08");
        assert_eq!(
            r.binary,
            "00000001 00000010 00000011 00000100 00000101 00000110 00000111 00001000"
        );
    }

    #[test]
    fn signed_and_float_readings() {
        let r = inspect(&[0xFF, 0xFF]);
        assert_eq!(r.i8, Some(-1));
        assert_eq!(r.i16_le, Some(-1));
        assert_eq!(r.i16_be, Some(-1));
        // 1.0f32 is 0x3F800000; little-endian bytes 00 00 80 3F.
        let r = inspect(&[0x00, 0x00, 0x80, 0x3F]);
        assert_eq!(r.f32_le, Some(1.0));
        // Big-endian 1.0f64 is 3F F0 followed by zeros.
        let r = inspect(&[0x3F, 0xF0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(r.f64_be, Some(1.0));
        // Little-endian 1.0f64: the same bytes reversed.
        let r = inspect(&[0, 0, 0, 0, 0, 0, 0xF0, 0x3F]);
        assert_eq!(r.f64_le, Some(1.0));
    }

    #[test]
    fn short_input_gives_partial_readings() {
        let r = inspect(&[0x2A]);
        assert_eq!(r.u8, Some(42));
        assert_eq!(r.u16_le, None);
        assert_eq!(r.u32_le, None);
        assert_eq!(r.hex, "2A");
        assert_eq!(r.binary, "00101010");
        // Three bytes: the u16 family works, the u32 family doesn't.
        let r = inspect(&[1, 2, 3]);
        assert_eq!(r.u16_le, Some(0x0201));
        assert_eq!(r.u32_le, None);
        assert_eq!(r.hex, "01 02 03");
    }

    #[test]
    fn hex_and_binary_show_at_most_eight_bytes() {
        let r = inspect(&[0xAB; 12]);
        assert_eq!(r.len, 12);
        assert_eq!(r.hex.matches(' ').count(), 7);
        assert!(r.hex.starts_with("AB AB"));
        assert_eq!(r.binary.split(' ').count(), 8);
    }

    #[test]
    fn date_conversion_epoch_and_leap_days() {
        assert_eq!(format_unix_seconds(0), "1970-01-01 00:00:00 UTC");
        // 2000 is a leap year (divisible by 400): Feb 29 exists.
        assert_eq!(format_unix_seconds(951_782_400), "2000-02-29 00:00:00 UTC");
        // An ordinary leap year.
        assert_eq!(
            format_unix_seconds(1_582_934_400),
            "2020-02-29 00:00:00 UTC"
        );
        // The day before the 2000 leap day, and the day after.
        assert_eq!(format_unix_seconds(951_696_000), "2000-02-28 00:00:00 UTC");
        assert_eq!(format_unix_seconds(951_868_800), "2000-03-01 00:00:00 UTC");
        // A time-of-day reading.
        assert_eq!(format_unix_seconds(3_661), "1970-01-01 01:01:01 UTC");
    }

    #[test]
    fn date_conversion_range_boundaries() {
        // The last sane second.
        assert_eq!(
            format_unix_seconds(4_102_444_799),
            "2099-12-31 23:59:59 UTC"
        );
        // 2100-01-01 and anything beyond is out of range.
        assert_eq!(format_unix_seconds(4_102_444_800), "—");
        assert_eq!(format_unix_seconds(u64::MAX), "—");
        // Milliseconds: the same boundaries, with the fraction shown.
        assert_eq!(format_unix_millis(0), "1970-01-01 00:00:00.000 UTC");
        assert_eq!(
            format_unix_millis(1_609_459_200_123),
            "2021-01-01 00:00:00.123 UTC"
        );
        assert_eq!(format_unix_millis(4_102_444_800_000), "—");
    }

    #[test]
    fn timestamp_fields_come_from_the_little_endian_values() {
        // 2021-01-01 00:00:00 UTC = 1609459200 = 0x5FEE6600.
        let r = inspect(&1_609_459_200u32.to_le_bytes());
        assert_eq!(r.unix_seconds_u32, "2021-01-01 00:00:00 UTC");
        // The u64 fields need 8 bytes; with only 4 they have no reading.
        assert_eq!(r.unix_seconds_u64, "—");
        let r = inspect(&1_609_459_200u64.to_le_bytes());
        assert_eq!(r.unix_seconds_u64, "2021-01-01 00:00:00 UTC");
        // The same u64 as milliseconds is in 1970 (about 18.6 days).
        assert_eq!(r.unix_millis_u64, "1970-01-19 15:04:19.200 UTC");
        // u64::MAX as seconds is far out of range.
        let r = inspect(&u64::MAX.to_le_bytes());
        assert_eq!(r.unix_seconds_u64, "—");
        assert_eq!(r.unix_millis_u64, "—");
    }

    #[test]
    fn guid_reading_uses_windows_storage_order() {
        // The GUID {00112233-4455-6677-8899-AABBCCDDEEFF} as it
        // lies in a file: the first three fields little-endian,
        // the rest in byte order.
        let bytes: [u8; 16] = [
            0x33, 0x22, 0x11, 0x00, 0x55, 0x44, 0x77, 0x66, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
            0xEE, 0xFF,
        ];
        let r = inspect(&bytes);
        assert_eq!(
            r.guid.as_deref(),
            Some("00112233-4455-6677-8899-AABBCCDDEEFF")
        );
        // An all-zero GUID formats too.
        let r = inspect(&[0u8; 16]);
        assert_eq!(
            r.guid.as_deref(),
            Some("00000000-0000-0000-0000-000000000000")
        );
        // Fifteen bytes is not a GUID.
        let r = inspect(&bytes[..15]);
        assert_eq!(r.guid, None);
    }

    #[test]
    fn filetime_reading_epoch_and_known_dates() {
        // Tick zero is the FILETIME epoch itself.
        assert_eq!(format_filetime(0), "1601-01-01 00:00:00 UTC");
        // 2021-01-01 00:00:00 UTC: (1609459200 + 11644473600) seconds
        // after 1601, in 100-ns ticks.
        let ticks_2021 = (1_609_459_200u64 + 11_644_473_600) * 10_000_000;
        assert_eq!(format_filetime(ticks_2021), "2021-01-01 00:00:00 UTC");
        // A pre-Unix-epoch date exercises the negative day counts:
        // 1900-01-01 00:00:00 UTC.
        let ticks_1900 = (11_644_473_600u64 - 2_208_988_800) * 10_000_000;
        assert_eq!(format_filetime(ticks_1900), "1900-01-01 00:00:00 UTC");
        // Sub-second ticks are dropped, not rounded.
        assert_eq!(
            format_filetime(ticks_2021 + 9_999_999),
            "2021-01-01 00:00:00 UTC"
        );
        // The report field reads the little-endian u64.
        let r = inspect(&ticks_2021.to_le_bytes());
        assert_eq!(r.filetime_u64, "2021-01-01 00:00:00 UTC");
    }

    #[test]
    fn filetime_reading_range_boundaries() {
        // The last sane tick: 2099-12-31 23:59:59.
        let last = (4_102_444_799u64 + 11_644_473_600) * 10_000_000;
        assert_eq!(format_filetime(last), "2099-12-31 23:59:59 UTC");
        // 2100-01-01 and anything beyond is out of range.
        assert_eq!(format_filetime(last + 10_000_000), "—");
        assert_eq!(format_filetime(u64::MAX), "—");
        // Fewer than 8 bytes: no reading.
        let r = inspect(&[0u8; 4]);
        assert_eq!(r.filetime_u64, "—");
    }

    #[test]
    fn utf8_preview_cases() {
        // Plain text passes through untouched.
        assert_eq!(inspect(b"Hello, world").utf8_preview, "Hello, world");
        // Multi-byte characters are kept whole.
        assert_eq!(
            inspect("héllo → wörld".as_bytes()).utf8_preview,
            "héllo → wörld"
        );
        // Control characters are dropped, the text around them kept.
        assert_eq!(inspect(b"ab\x00c\r\nd\t").utf8_preview, "abcd");
        // The run stops at the first invalid byte.
        assert_eq!(inspect(b"abc\xFFdef").utf8_preview, "abc");
        // A truncated multi-byte character ends the run cleanly.
        assert_eq!(inspect(&[b'a', 0xC3]).utf8_preview, "a");
        // Nothing printable: the no-reading marker.
        assert_eq!(inspect(&[0xFF, 0xFE]).utf8_preview, "—");
        assert_eq!(inspect(&[0x00, 0x01, 0x02]).utf8_preview, "—");
        assert_eq!(inspect(b"").utf8_preview, "—");
        // The preview caps at 32 characters (characters, not bytes).
        let long = "é".repeat(40);
        let preview = inspect(long.as_bytes()).utf8_preview;
        assert_eq!(preview.chars().count(), 32);
    }

    #[test]
    fn days_from_civil_inverts_civil_from_days() {
        // Anchors known independently of the algorithm.
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1601, 1, 1), -134_774);
        assert_eq!(days_from_civil(2026, 11, 1), 20_758);
        // Round-trip across era boundaries and leap cases.
        for days in [-134_774, -1, 0, 1, 20_758, 47_482] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "day {days}");
        }
        for (y, m, d) in [(2000, 2, 29), (1900, 3, 1), (2024, 2, 29), (2099, 12, 31)] {
            assert_eq!(civil_from_days(days_from_civil(y, m, d)), (y, m, d));
        }
    }
}
