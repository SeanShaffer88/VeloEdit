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

/// How a hex row is laid out (roadmap items 13/14): the row width,
/// the visual grouping, and the byte order inside a group. The
/// grouping and order are display-only — the file's bytes never
/// move, and the caret stays a plain byte offset under every
/// layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HexLayout {
    /// Bytes on each row.
    pub bytes_per_row: u64,
    /// Bytes per visual group: 1, 2, 4, or 8.
    pub group: usize,
    /// Show each group's bytes in reverse (big-endian) order, so a
    /// group reads as one big-endian number.
    pub big_endian: bool,
}

impl Default for HexLayout {
    /// The classic layout: 16 bytes per row, ungrouped, file order.
    fn default() -> Self {
        HexLayout {
            bytes_per_row: BYTES_PER_ROW,
            group: 1,
            big_endian: false,
        }
    }
}

impl HexLayout {
    /// The group size clamped to the supported set (1, 2, 4, 8);
    /// anything else behaves as 1.
    fn group_size(&self) -> usize {
        match self.group {
            2 | 4 | 8 => self.group,
            _ => 1,
        }
    }
}

/// Byte offset of the first byte of `row`. Saturates at `u64::MAX` for
/// absurd row indices instead of wrapping.
pub fn row_offset(row: u64) -> u64 {
    row_offset_w(row, BYTES_PER_ROW)
}

/// Byte offset of the first byte of `row` at `width` bytes per row.
pub fn row_offset_w(row: u64, width: u64) -> u64 {
    row.saturating_mul(width.max(1))
}

/// Row containing `offset`.
pub fn row_for_offset(offset: u64) -> u64 {
    row_for_offset_w(offset, BYTES_PER_ROW)
}

/// Row containing `offset` at `width` bytes per row.
pub fn row_for_offset_w(offset: u64, width: u64) -> u64 {
    offset / width.max(1)
}

/// Number of rows needed to show `total_len` bytes. The final partial row
/// counts; an empty file has zero rows.
pub fn row_count(total_len: u64) -> u64 {
    row_count_w(total_len, BYTES_PER_ROW)
}

/// Number of rows needed to show `total_len` bytes at `width`
/// bytes per row.
pub fn row_count_w(total_len: u64, width: u64) -> u64 {
    total_len.div_ceil(width.max(1))
}

/// Bytes shown on the final row of a file of `total_len` bytes: a full row
/// when the length is an exact multiple, otherwise the remainder. Zero for
/// an empty file.
pub fn last_row_len(total_len: u64) -> u64 {
    last_row_len_w(total_len, BYTES_PER_ROW)
}

/// Bytes shown on the final row at `width` bytes per row.
pub fn last_row_len_w(total_len: u64, width: u64) -> u64 {
    let width = width.max(1);
    if total_len == 0 {
        0
    } else {
        let rem = total_len % width;
        if rem == 0 {
            width
        } else {
            rem
        }
    }
}

/// Spaces following the byte at display position `pos` in a row of
/// `count` bytes: one space inside a group, two at a group
/// boundary. With single-byte groups the classic layout has no
/// group boundaries, so instead a wider gap splits the row in
/// half (after byte 8 of 16, and pro rata at other even widths).
fn spaces_after(pos: usize, count: usize, group: usize) -> usize {
    if pos + 1 >= count {
        return 0;
    }
    if group <= 1 {
        if count >= 4 && count % 2 == 0 && pos + 1 == count / 2 {
            2
        } else {
            1
        }
    } else if (pos + 1) % group == 0 {
        2
    } else {
        1
    }
}

/// The file index (within the row) of the byte shown at display
/// position `pos`, and its inverse: with big-endian groups the
/// bytes inside each group trade places; in file order both are
/// the identity. The final group of a short row reverses within
/// its actual length.
fn display_swap(index: usize, count: usize, layout: &HexLayout) -> usize {
    let group = layout.group_size();
    if !layout.big_endian || group <= 1 || count == 0 {
        return index.min(count.saturating_sub(1));
    }
    let start = (index / group) * group;
    let len = group.min(count - start);
    start + (len - 1 - (index - start))
}

/// The file index of the byte shown at display position `pos` in
/// a row of `count` bytes.
pub fn byte_at_display_pos(pos: usize, count: usize, layout: &HexLayout) -> usize {
    display_swap(pos, count, layout)
}

/// The display position of the byte at file index `idx` in a row
/// of `count` bytes (the inverse of [`byte_at_display_pos`]).
pub fn display_pos_of_byte(idx: usize, count: usize, layout: &HexLayout) -> usize {
    display_swap(idx, count, layout)
}

/// Character column where display position `pos` starts in the
/// formatted row text: every earlier position contributes its two
/// digits plus its following spaces.
fn char_col_of_pos(pos: usize, count: usize, layout: &HexLayout) -> usize {
    let group = layout.group_size();
    (0..pos).map(|p| 2 + spaces_after(p, count, group)).sum()
}

/// Character column where the digits of the byte at file index
/// `idx` start in a row of `count` bytes — where the shell draws
/// that byte's selection cell and caret box.
pub fn byte_char_col(idx: usize, count: usize, layout: &HexLayout) -> usize {
    char_col_of_pos(display_pos_of_byte(idx, count, layout), count, layout)
}

/// Character count of the formatted hex text for a row of `count`
/// bytes (the last byte carries no trailing spaces).
pub fn hex_text_len(count: usize, layout: &HexLayout) -> usize {
    if count == 0 {
        0
    } else {
        char_col_of_pos(count - 1, count, layout) + 2
    }
}

/// The byte index a click at character column `col` lands on, in
/// a row of `count` bytes: a column on a byte's digits gives that
/// byte, a column in the gap after a byte gives the offset just
/// past it, and a column beyond the row gives `count` (the caret
/// position after the last byte). Always in `0..=count`.
pub fn byte_at_char_col(col: usize, count: usize, layout: &HexLayout) -> usize {
    let group = layout.group_size();
    let mut start = 0usize;
    for pos in 0..count {
        let byte = byte_at_display_pos(pos, count, layout);
        if col < start + 2 {
            return byte;
        }
        if col < start + 2 + spaces_after(pos, count, group) {
            return (byte + 1).min(count);
        }
        start += 2 + spaces_after(pos, count, group);
    }
    count
}

/// Format one row's bytes as the hex display text under `layout`:
/// two upper-case digits per byte, spaced by [`spaces_after`],
/// with each group's bytes reversed when the layout is
/// big-endian. At the default layout this is exactly the classic
/// string (single spaces, a wider gap after the eighth byte).
pub fn format_hex(bytes: &[u8], layout: &HexLayout) -> String {
    let count = bytes.len();
    let group = layout.group_size();
    let mut out = String::with_capacity(hex_text_len(count, layout));
    for pos in 0..count {
        if pos > 0 {
            for _ in 0..spaces_after(pos - 1, count, group) {
                out.push(' ');
            }
        }
        let b = bytes[byte_at_display_pos(pos, count, layout)];
        out.push_str(&format!("{b:02X}"));
    }
    out
}

/// Row widths the fit-to-window mode chooses from, ascending.
const FIT_WIDTHS: [u64; 9] = [8, 16, 24, 32, 48, 64, 96, 128, 256];

/// The widest row width that fits `avail_chars` character cells:
/// a row needs its formatted hex text, a two-cell gap, and one
/// cell per byte for the ASCII column. Falls back to the smallest
/// width when even that does not fit.
pub fn fit_bytes_per_row(avail_chars: usize, group: usize) -> u64 {
    let probe = HexLayout {
        bytes_per_row: 0,
        group,
        big_endian: false,
    };
    let mut best = FIT_WIDTHS[0];
    for &width in &FIT_WIDTHS {
        let needed = hex_text_len(width as usize, &probe) + 2 + width as usize;
        if needed <= avail_chars {
            best = width;
        } else {
            break;
        }
    }
    best
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

    #[test]
    fn width_parameterised_row_math() {
        assert_eq!(row_offset_w(3, 8), 24);
        assert_eq!(row_offset_w(3, 32), 96);
        assert_eq!(row_offset_w(u64::MAX, 32), u64::MAX); // saturates
        assert_eq!(row_for_offset_w(24, 8), 3);
        assert_eq!(row_for_offset_w(31, 8), 3);
        assert_eq!(row_for_offset_w(96, 32), 3);
        assert_eq!(row_count_w(0, 8), 0);
        assert_eq!(row_count_w(17, 8), 3);
        assert_eq!(row_count_w(64, 32), 2);
        assert_eq!(row_count_w(100 * 1024 * 1024 * 1024, 32), 3_355_443_200);
        assert_eq!(last_row_len_w(0, 8), 0);
        assert_eq!(last_row_len_w(16, 8), 8);
        assert_eq!(last_row_len_w(21, 8), 5);
        // A zero width is treated as 1 rather than dividing by zero.
        assert_eq!(row_count_w(5, 0), 5);
        assert_eq!(row_for_offset_w(5, 0), 5);
    }

    fn layout(group: usize, big_endian: bool) -> HexLayout {
        HexLayout {
            bytes_per_row: 16,
            group,
            big_endian,
        }
    }

    #[test]
    fn default_layout_format_is_the_classic_string() {
        let bytes: Vec<u8> = (0..16).collect();
        let text = format_hex(&bytes, &HexLayout::default());
        assert_eq!(text, "00 01 02 03 04 05 06 07  08 09 0A 0B 0C 0D 0E 0F");
        assert_eq!(text.len(), hex_text_len(16, &HexLayout::default()));
        assert_eq!(hex_text_len(16, &HexLayout::default()), 48);
        assert_eq!(hex_text_len(0, &HexLayout::default()), 0);
        assert_eq!(format_hex(&[], &HexLayout::default()), "");
        // A short row: no mid-row break below four bytes, and the
        // break lands after half the row when it applies.
        assert_eq!(format_hex(&[0xAB, 0xCD], &HexLayout::default()), "AB CD");
        assert_eq!(
            format_hex(&[1, 2, 3, 4], &HexLayout::default()),
            "01 02  03 04"
        );
    }

    #[test]
    fn grouped_formats_space_and_reverse() {
        let bytes: Vec<u8> = (0..8).collect();
        // Words, little-endian display: pairs with a wide gap.
        assert_eq!(
            format_hex(&bytes, &layout(2, false)),
            "00 01  02 03  04 05  06 07"
        );
        // Words, big-endian display: each pair reads as one number.
        assert_eq!(
            format_hex(&bytes, &layout(2, true)),
            "01 00  03 02  05 04  07 06"
        );
        // Dwords big-endian.
        assert_eq!(
            format_hex(&bytes, &layout(4, true)),
            "03 02 01 00  07 06 05 04"
        );
        // Qword big-endian: the whole row is one number.
        assert_eq!(
            format_hex(&bytes, &layout(8, true)),
            "07 06 05 04 03 02 01 00"
        );
        // A short final group reverses within its own length only.
        assert_eq!(format_hex(&bytes[..5], &layout(4, true)), "03 02 01 00  04");
        assert_eq!(
            format_hex(&bytes[..6], &layout(4, true)),
            "03 02 01 00  05 04"
        );
        // An unsupported group size behaves as 1.
        assert_eq!(format_hex(&bytes[..4], &layout(3, false)), "00 01  02 03");
    }

    #[test]
    fn display_positions_swap_within_groups() {
        let be4 = layout(4, true);
        // Full groups reverse; both directions agree (an involution).
        for i in 0..8 {
            let pos = display_pos_of_byte(i, 8, &be4);
            assert_eq!(byte_at_display_pos(pos, 8, &be4), i);
        }
        assert_eq!(display_pos_of_byte(0, 8, &be4), 3);
        assert_eq!(display_pos_of_byte(3, 8, &be4), 0);
        assert_eq!(display_pos_of_byte(4, 8, &be4), 7);
        // Little-endian display never swaps.
        let le4 = layout(4, false);
        for i in 0..8 {
            assert_eq!(display_pos_of_byte(i, 8, &le4), i);
            assert_eq!(byte_at_display_pos(i, 8, &le4), i);
        }
        // The partial group at the end of a short row.
        assert_eq!(display_pos_of_byte(4, 6, &be4), 5);
        assert_eq!(display_pos_of_byte(5, 6, &be4), 4);
    }

    #[test]
    fn byte_char_col_matches_the_formatted_text() {
        // Every byte's digits sit in the formatted string exactly
        // where byte_char_col says, under every layout.
        let bytes: Vec<u8> = (0x10..0x20).collect();
        for lay in [
            HexLayout::default(),
            layout(1, true), // big_endian alone never swaps
            layout(2, false),
            layout(2, true),
            layout(4, true),
            layout(8, true),
        ] {
            let text = format_hex(&bytes, &lay);
            assert_eq!(text.len(), hex_text_len(bytes.len(), &lay));
            for (idx, &b) in bytes.iter().enumerate() {
                let col = byte_char_col(idx, bytes.len(), &lay);
                assert_eq!(
                    &text[col..col + 2],
                    &format!("{b:02X}"),
                    "layout {lay:?}, byte {idx}"
                );
            }
        }
    }

    #[test]
    fn byte_at_char_col_hit_testing() {
        let lay = HexLayout::default();
        // Classic 16-byte row: digits at 0,3,6,…,24 (no gap) then
        // 27,30,… after the mid-row break.
        assert_eq!(byte_at_char_col(0, 16, &lay), 0);
        assert_eq!(byte_at_char_col(1, 16, &lay), 0);
        assert_eq!(byte_at_char_col(2, 16, &lay), 1); // the gap after byte 0
        assert_eq!(byte_at_char_col(24, 16, &lay), 8); // inside the break gap
        assert_eq!(byte_at_char_col(25, 16, &lay), 8); // first digit after the break
        assert_eq!(byte_at_char_col(47, 16, &lay), 15);
        assert_eq!(byte_at_char_col(48, 16, &lay), 16); // past the last byte
        assert_eq!(byte_at_char_col(1000, 16, &lay), 16);
        assert_eq!(byte_at_char_col(0, 0, &lay), 0);
        // Grouped big-endian: clicking the leftmost digits selects
        // the byte displayed there, not file byte 0.
        let be2 = layout(2, true);
        assert_eq!(byte_at_char_col(0, 8, &be2), 1);
        assert_eq!(byte_at_char_col(3, 8, &be2), 0);
        // The double-space group gap after a pair advances past
        // the byte shown just before it.
        assert_eq!(byte_at_char_col(6, 8, &be2), 1);
    }

    #[test]
    fn fit_width_picks_the_largest_that_fits() {
        // 16 bytes at group 1: 48 hex chars + 2 gap + 16 ASCII = 66.
        assert_eq!(fit_bytes_per_row(66, 1), 16);
        assert_eq!(fit_bytes_per_row(65, 1), 8);
        // 24 bytes: 72 + 2 + 24 = 98.
        assert_eq!(fit_bytes_per_row(98, 1), 24);
        assert_eq!(fit_bytes_per_row(97, 1), 16);
        // Nothing fits: the smallest width is the fallback.
        assert_eq!(fit_bytes_per_row(10, 1), 8);
        // A wide window reaches the top of the table.
        assert_eq!(fit_bytes_per_row(10_000, 1), 256);
        // Grouping adds boundary gaps, so fewer bytes fit: at
        // group 8, 32 bytes need 98 + 2 + 32 = 132 cells.
        assert_eq!(fit_bytes_per_row(132, 8), 32);
        assert_eq!(fit_bytes_per_row(131, 8), 24);
    }
}
