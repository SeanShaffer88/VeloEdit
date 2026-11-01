//! Hex / binary view arithmetic (SPEC.md section 3.1, 3.2).
//!
//! Hex mode needs no line index: with a fixed number of bytes per row, the
//! byte offset of a row is O(1) arithmetic (`row * bytes_per_row`).
//!
//! Scrolling is the subtle part. A Win32 scrollbar range is a 32-bit value,
//! which truncates past 2 billion units — far below a 100+ GB file size —
//! so thumb placement uses the 64-bit normalized ratio from SPEC.md
//! section 3.2:
//!
//! ```text
//! Thumb Position = floor(Current Offset / Total Length * Track Range)
//! ```
//!
//! The ratio is evaluated in `u128` fixed point so neither the multiply nor
//! the divide can overflow for any 64-bit file size and track range.

/// Bytes shown on each hex row (SPEC.md section 3.2: offsets 00-0F).
pub const BYTES_PER_ROW: u64 = 16;

/// Byte offset of the first byte of `row`. Saturates at `u64::MAX` for
/// absurd row indices instead of wrapping.
pub fn row_offset(row: u64) -> u64 {
    row.saturating_mul(BYTES_PER_ROW)
}

/// Row containing `offset`.
pub fn row_for_offset(offset: u64) -> u64 {
    offset / BYTES_PER_ROW
}

/// Number of rows needed to show `total_len` bytes. The final partial row
/// counts; an empty file has zero rows.
pub fn row_count(total_len: u64) -> u64 {
    total_len.div_ceil(BYTES_PER_ROW)
}

/// Bytes shown on the final row of a file of `total_len` bytes: a full row
/// when the length is an exact multiple, otherwise the remainder. Zero for
/// an empty file.
pub fn last_row_len(total_len: u64) -> u64 {
    if total_len == 0 {
        0
    } else {
        let rem = total_len % BYTES_PER_ROW;
        if rem == 0 {
            BYTES_PER_ROW
        } else {
            rem
        }
    }
}

/// Scroll thumb position for `current_offset` within a file of
/// `total_length`, on a scrollbar track of `track_range` units
/// (SPEC.md section 3.2). The result is clamped to `track_range`;
/// a zero-length file or a zero-size track always yields 0.
pub fn thumb_position(current_offset: u64, total_length: u64, track_range: u64) -> u64 {
    if total_length == 0 || track_range == 0 {
        return 0;
    }
    let clamped = current_offset.min(total_length);
    let scaled = (clamped as u128 * track_range as u128) / total_length as u128;
    scaled as u64 // <= track_range by construction, so it always fits.
}

/// Inverse of [`thumb_position`]: the byte offset a thumb resting at
/// `thumb` on a track of `track_range` units points at, for a file of
/// `total_length`. Used when the user drags the scrollbar. Clamped to
/// `total_length`.
pub fn offset_for_thumb(thumb: u64, total_length: u64, track_range: u64) -> u64 {
    if total_length == 0 || track_range == 0 {
        return 0;
    }
    let clamped = thumb.min(track_range);
    let scaled = (clamped as u128 * total_length as u128) / track_range as u128;
    scaled as u64 // <= total_length by construction.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_math_is_o1_arithmetic() {
        assert_eq!(row_offset(0), 0);
        assert_eq!(row_offset(1), 16);
        assert_eq!(row_offset(0x10), 0x100);
        assert_eq!(row_for_offset(0), 0);
        assert_eq!(row_for_offset(15), 0);
        assert_eq!(row_for_offset(16), 1);
        assert_eq!(row_for_offset(0x1234), 0x123);
        assert_eq!(row_offset(u64::MAX), u64::MAX); // saturates, never wraps
    }

    #[test]
    fn row_count_rounds_up() {
        assert_eq!(row_count(0), 0);
        assert_eq!(row_count(1), 1);
        assert_eq!(row_count(16), 1);
        assert_eq!(row_count(17), 2);
        assert_eq!(row_count(100 * 1024 * 1024 * 1024), 6_710_886_400);
    }

    #[test]
    fn last_row_len_cases() {
        assert_eq!(last_row_len(0), 0);
        assert_eq!(last_row_len(5), 5);
        assert_eq!(last_row_len(16), 16);
        assert_eq!(last_row_len(31), 15);
        assert_eq!(last_row_len(32), 16);
    }

    #[test]
    fn thumb_position_edges() {
        assert_eq!(thumb_position(0, 0, 100), 0); // empty file
        assert_eq!(thumb_position(0, 1000, 0), 0); // no track
        assert_eq!(thumb_position(0, 1000, 100), 0); // at start
        assert_eq!(thumb_position(500, 1000, 100), 50); // halfway
        assert_eq!(thumb_position(1000, 1000, 100), 100); // exactly at end
        assert_eq!(thumb_position(5000, 1000, 100), 100); // past end clamps
        assert_eq!(thumb_position(1, 3, 10), 3); // floor(1/3 * 10)
    }

    #[test]
    fn thumb_position_beyond_32_bit_sizes() {
        // A 100 GB file: offsets and lengths far past the 32-bit scrollbar
        // limit that SPEC.md section 3.2 calls out.
        let total: u64 = 100 * 1024 * 1024 * 1024;
        let track: u64 = 10_000;
        assert_eq!(thumb_position(total / 2, total, track), 5000);
        assert_eq!(thumb_position(total, total, track), track);
        assert_eq!(thumb_position(total - 1, total, track), 9999);
        // Extreme: max-size file and max-size track must not overflow.
        assert_eq!(thumb_position(u64::MAX, u64::MAX, u64::MAX), u64::MAX);
        assert_eq!(thumb_position(u64::MAX / 2, u64::MAX, 1000), 499);
    }

    #[test]
    fn thumb_and_offset_are_inverses_at_track_steps() {
        let total: u64 = 1_000_000;
        let track: u64 = 500;
        for thumb in [0u64, 1, 125, 250, 499, 500] {
            let offset = offset_for_thumb(thumb, total, track);
            assert_eq!(thumb_position(offset, total, track), thumb);
        }
        assert_eq!(offset_for_thumb(0, 0, 500), 0);
        assert_eq!(offset_for_thumb(9999, total, track), total); // clamps
    }
}
