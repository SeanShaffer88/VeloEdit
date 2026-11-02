//! Pane geometry and synchronisation maths (roadmap item 28 —
//! the text/hex split view — and the side-by-side streaming
//! diff, slice R5c).
//!
//! Two features share one idea: two panes side by side whose
//! scroll positions are tied together. The split view ties a
//! document's text and hex representations by a byte-offset
//! anchor; the compare ties two documents by a line (or hex-row)
//! offset — the *delta* between their first visible lines. This
//! module is the pure arithmetic over that idea — divider
//! placement, the anchor mapping between an offset and a hex
//! row, the delta-follow rule, and difference-region navigation
//! over a [`crate::linediff::LineDiffReport`]. It is deliberately
//! free of any window or editor type so the whole model is
//! host-testable; the shell (`gui_win`) owns the state and the
//! painting.

use crate::linediff::LineDiffRegion;

/// The divider position as a fraction of the canvas span: the
/// persisted form of where the user left the divider. Kept
/// inside [MIN_RATIO, MAX_RATIO] so neither pane can be dragged
/// (or restored) to nothing.
pub const MIN_RATIO: f32 = 0.15;
/// See [`MIN_RATIO`].
pub const MAX_RATIO: f32 = 0.85;
/// Where the divider sits until the user moves it.
pub const DEFAULT_RATIO: f32 = 0.5;

/// Clamp a stored or dragged divider ratio to the usable range.
/// A non-finite value (a hand-edited settings file) restores the
/// default rather than poisoning the layout.
pub fn clamp_ratio(ratio: f32) -> f32 {
    if !ratio.is_finite() {
        return DEFAULT_RATIO;
    }
    ratio.clamp(MIN_RATIO, MAX_RATIO)
}

/// The divider's x for a canvas spanning `[left, right]`: the
/// ratio point, clamped so each side keeps at least its minimum
/// width. When the span cannot satisfy both minimums (a very
/// narrow window), the divider takes the middle — both panes
/// stay partly usable instead of one vanishing.
pub fn divider_x(left: f32, right: f32, ratio: f32, min_left: f32, min_right: f32) -> f32 {
    let span = (right - left).max(0.0);
    let want = left + span * clamp_ratio(ratio);
    let lo = left + min_left.min(span);
    let hi = right - min_right.min(span);
    if lo > hi {
        return left + span / 2.0;
    }
    want.clamp(lo, hi)
}

/// The ratio a drag to `x` asks for, as a plain span fraction
/// (not clamped to the ratio range — [`divider_x`] applies the
/// minimums, so the stored ratio may sit outside
/// [MIN_RATIO, MAX_RATIO] only in that it is re-clamped on every
/// use; the shell stores this value and the layout stays stable
/// because `divider_x` is idempotent for it).
pub fn ratio_for_x(left: f32, right: f32, x: f32) -> f32 {
    let span = right - left;
    if span <= 0.0 {
        return DEFAULT_RATIO;
    }
    (x - left) / span
}

/// The hex row whose span contains byte `offset` — the split
/// view's anchor mapping from a text line's start offset to the
/// hex pane's top row. A zero row width (no layout yet) maps
/// everything to row 0 rather than dividing by zero.
pub fn hex_row_for_offset(offset: u64, bytes_per_row: u64) -> u64 {
    crate::hexview::row_for_offset_w(offset, bytes_per_row.max(1))
}

/// The byte offset at the start of hex row `row` — the anchor
/// mapping back from the hex pane's top row to the text pane's
/// top line (the line containing this offset).
pub fn offset_for_hex_row(row: u64, bytes_per_row: u64) -> u64 {
    crate::hexview::row_offset_w(row, bytes_per_row.max(1))
}

/// The scroll-follow rule for tied panes: when the anchor pane's
/// first visible unit (line or hex row) is `anchor_first`, the
/// other pane's first unit is `anchor_first + delta`, clamped to
/// its own document (`total` units; an empty document's only
/// position is 0). The delta is what pane jumps set and plain
/// scrolling preserves.
pub fn follow_first(anchor_first: u64, delta: i64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    let want = anchor_first as i128 + delta as i128;
    want.clamp(0, (total - 1) as i128) as u64
}

/// The delta a jump to a paired position establishes: jumping to
/// left unit `anchor_start` / right unit `other_start` makes the
/// panes follow from exactly that pairing afterwards.
pub fn jump_delta(anchor_start: u64, other_start: u64) -> i64 {
    other_start as i64 - anchor_start as i64
}

/// Whether `line` falls in a region's span on one side. A
/// zero-count side (a pure insertion or deletion) matches its
/// start line exactly — the band paints on the line the gap
/// sits before, the convention the diff panes follow.
fn span_contains(start: u64, count: u64, line: u64) -> bool {
    if count == 0 {
        line == start
    } else {
        line >= start && line < start + count
    }
}

/// The region whose LEFT span contains `line`, if any. Regions
/// are ascending and non-overlapping, so a binary search over
/// the starts finds the only candidate.
pub fn region_index_at_left(regions: &[LineDiffRegion], line: u64) -> Option<usize> {
    region_index_at(regions, line, true)
}

/// The region whose RIGHT span contains `line`, if any.
pub fn region_index_at_right(regions: &[LineDiffRegion], line: u64) -> Option<usize> {
    region_index_at(regions, line, false)
}

fn region_index_at(regions: &[LineDiffRegion], line: u64, left: bool) -> Option<usize> {
    let start_of = |r: &LineDiffRegion| if left { r.left_start } else { r.right_start };
    let count_of = |r: &LineDiffRegion| if left { r.left_count } else { r.right_count };
    // The last region starting at or before the line is the only
    // one that can contain it.
    let idx = regions.partition_point(|r| start_of(r) <= line);
    if idx == 0 {
        return None;
    }
    let region = &regions[idx - 1];
    if span_contains(start_of(region), count_of(region), line) {
        Some(idx - 1)
    } else {
        None
    }
}

/// Next Difference: the region index to jump to from
/// `caret_line` (the left document's line), given the region the
/// user is parked on (`current`, from the last jump). The rules,
/// in order: a caret parked inside the current region advances
/// to the following region; a caret inside some other region
/// goes to that region's start; otherwise the first region
/// starting after the caret; and past the last region the
/// navigation wraps to the first. `None` when there are no
/// regions.
pub fn next_region(
    regions: &[LineDiffRegion],
    caret_line: u64,
    current: Option<usize>,
) -> Option<usize> {
    if regions.is_empty() {
        return None;
    }
    if let Some(cur) = current {
        if let Some(region) = regions.get(cur) {
            if span_contains(region.left_start, region.left_count, caret_line) {
                return Some((cur + 1) % regions.len());
            }
        }
    }
    if let Some(idx) = region_index_at_left(regions, caret_line) {
        return Some(idx);
    }
    let after = regions.partition_point(|r| r.left_start <= caret_line);
    if after < regions.len() {
        return Some(after);
    }
    Some(0)
}

/// Previous Difference: the mirror of [`next_region`]. A caret
/// parked exactly at the current region's start steps to the
/// region before it; a caret inside a region (past its start)
/// goes to that region's start; otherwise the last region
/// starting before the caret; and before the first region the
/// navigation wraps to the last.
pub fn prev_region(
    regions: &[LineDiffRegion],
    caret_line: u64,
    current: Option<usize>,
) -> Option<usize> {
    if regions.is_empty() {
        return None;
    }
    let n = regions.len();
    if let Some(cur) = current {
        if let Some(region) = regions.get(cur) {
            if caret_line == region.left_start {
                return Some((cur + n - 1) % n);
            }
        }
    }
    if let Some(idx) = region_index_at_left(regions, caret_line) {
        let region = &regions[idx];
        if caret_line > region.left_start || current != Some(idx) {
            return Some(idx);
        }
        return Some((idx + n - 1) % n);
    }
    let before = regions.partition_point(|r| r.left_start < caret_line);
    if before > 0 {
        return Some(before - 1);
    }
    Some(n - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(
        left_start: u64,
        left_count: u64,
        right_start: u64,
        right_count: u64,
    ) -> LineDiffRegion {
        LineDiffRegion {
            left_start,
            left_count,
            right_start,
            right_count,
        }
    }

    /// Three regions with gaps, one of them a pure insertion on
    /// the left (zero left count) — the shapes navigation and
    /// painting must handle.
    fn sample() -> Vec<LineDiffRegion> {
        vec![
            region(2, 3, 2, 3),
            region(10, 0, 12, 4),
            region(20, 5, 24, 2),
        ]
    }

    #[test]
    fn ratio_clamps_and_defaults() {
        assert_eq!(clamp_ratio(0.5), 0.5);
        assert_eq!(clamp_ratio(0.0), MIN_RATIO);
        assert_eq!(clamp_ratio(1.0), MAX_RATIO);
        assert_eq!(clamp_ratio(f32::NAN), DEFAULT_RATIO);
        assert_eq!(clamp_ratio(f32::INFINITY), DEFAULT_RATIO);
        assert_eq!(clamp_ratio(-3.0), MIN_RATIO);
    }

    #[test]
    fn divider_respects_minimums() {
        // Wide canvas: the ratio rules, subject to the minimums.
        assert_eq!(divider_x(0.0, 1000.0, 0.5, 180.0, 180.0), 500.0);
        assert_eq!(divider_x(0.0, 1000.0, 0.15, 180.0, 180.0), 180.0);
        assert_eq!(divider_x(0.0, 1000.0, 0.85, 180.0, 180.0), 820.0);
        // A canvas offset from zero keeps the same shape.
        assert_eq!(divider_x(100.0, 1100.0, 0.5, 180.0, 180.0), 600.0);
        // Too narrow for both minimums: the middle, not a vanish.
        assert_eq!(divider_x(0.0, 200.0, 0.5, 180.0, 180.0), 100.0);
        // Degenerate spans stay inside themselves.
        assert_eq!(divider_x(50.0, 50.0, 0.5, 180.0, 180.0), 50.0);
    }

    #[test]
    fn ratio_round_trips_through_the_divider() {
        for want in [300.0f32, 500.0, 640.0] {
            let ratio = ratio_for_x(0.0, 1000.0, want);
            assert_eq!(divider_x(0.0, 1000.0, ratio, 0.0, 0.0), want);
        }
        assert_eq!(ratio_for_x(0.0, 0.0, 10.0), DEFAULT_RATIO);
    }

    #[test]
    fn hex_anchor_mapping_round_trips() {
        for width in [1u64, 8, 16, 24, 64] {
            for offset in [0u64, 1, 15, 16, 17, 255, 4096, 1_000_003] {
                let row = hex_row_for_offset(offset, width);
                let start = offset_for_hex_row(row, width);
                assert!(
                    start <= offset && offset < start + width,
                    "w={width} o={offset}"
                );
                assert_eq!(hex_row_for_offset(start, width), row);
            }
        }
        // Row boundaries are exact.
        assert_eq!(hex_row_for_offset(32, 16), 2);
        assert_eq!(hex_row_for_offset(31, 16), 1);
        assert_eq!(offset_for_hex_row(2, 16), 32);
        // A zero width never divides by zero.
        assert_eq!(hex_row_for_offset(10, 0), 10);
        assert_eq!(offset_for_hex_row(3, 0), 3);
    }

    #[test]
    fn follow_first_applies_and_clamps_the_delta() {
        assert_eq!(follow_first(10, 0, 100), 10);
        assert_eq!(follow_first(10, 5, 100), 15);
        assert_eq!(follow_first(10, -4, 100), 6);
        // Clamped at both ends of the other document.
        assert_eq!(follow_first(10, -40, 100), 0);
        assert_eq!(follow_first(95, 40, 100), 99);
        // An empty other document only has position 0.
        assert_eq!(follow_first(10, 5, 0), 0);
        // A one-line document likewise.
        assert_eq!(follow_first(10, 5, 1), 0);
    }

    #[test]
    fn jump_delta_pairs_the_starts() {
        assert_eq!(jump_delta(20, 24), 4);
        assert_eq!(jump_delta(20, 12), -8);
        assert_eq!(jump_delta(0, 0), 0);
        // After the jump the follow rule reproduces the pairing.
        assert_eq!(follow_first(20, jump_delta(20, 24), 1000), 24);
    }

    #[test]
    fn region_lookup_left_and_right() {
        let regions = sample();
        // Inside the first region (left 2..5, right 2..5).
        assert_eq!(region_index_at_left(&regions, 2), Some(0));
        assert_eq!(region_index_at_left(&regions, 4), Some(0));
        assert_eq!(region_index_at_right(&regions, 3), Some(0));
        // The gap between regions belongs to none.
        assert_eq!(region_index_at_left(&regions, 5), None);
        assert_eq!(region_index_at_left(&regions, 9), None);
        // The zero-count region matches exactly its start line.
        assert_eq!(region_index_at_left(&regions, 10), Some(1));
        assert_eq!(region_index_at_left(&regions, 11), None);
        // ...while its right span covers four lines.
        assert_eq!(region_index_at_right(&regions, 12), Some(1));
        assert_eq!(region_index_at_right(&regions, 15), Some(1));
        assert_eq!(region_index_at_right(&regions, 16), None);
        // Last region and beyond.
        assert_eq!(region_index_at_left(&regions, 24), Some(2));
        assert_eq!(region_index_at_left(&regions, 25), None);
        assert_eq!(region_index_at_left(&regions, 0), None);
        assert_eq!(region_index_at_left(&[], 3), None);
    }

    #[test]
    fn next_region_walks_and_wraps() {
        let regions = sample();
        // From before everything: the first region.
        assert_eq!(next_region(&regions, 0, None), Some(0));
        // Parked at the first region's start: advance.
        assert_eq!(next_region(&regions, 2, Some(0)), Some(1));
        // Inside the first region but not parked: go to its start
        // (the jump lands the caret at the start).
        assert_eq!(next_region(&regions, 3, None), Some(0));
        // Inside it and parked: advance.
        assert_eq!(next_region(&regions, 3, Some(0)), Some(1));
        // In a gap: the next region after the caret.
        assert_eq!(next_region(&regions, 7, None), Some(1));
        // At the zero-count region's start, parked: advance.
        assert_eq!(next_region(&regions, 10, Some(1)), Some(2));
        // Past the end: wrap to the first.
        assert_eq!(next_region(&regions, 99, None), Some(0));
        assert_eq!(next_region(&regions, 24, Some(2)), Some(0));
        assert_eq!(next_region(&[], 0, None), None);
    }

    #[test]
    fn prev_region_walks_and_wraps() {
        let regions = sample();
        // Parked at a region's start: step back.
        assert_eq!(prev_region(&regions, 20, Some(2)), Some(1));
        assert_eq!(prev_region(&regions, 2, Some(0)), Some(2));
        // Inside a region, past its start: back to its start.
        assert_eq!(prev_region(&regions, 22, None), Some(2));
        assert_eq!(prev_region(&regions, 22, Some(2)), Some(2));
        // In a gap: the region before the caret.
        assert_eq!(prev_region(&regions, 18, None), Some(1));
        // Before everything: wrap to the last.
        assert_eq!(prev_region(&regions, 0, None), Some(2));
        assert_eq!(prev_region(&[], 0, None), None);
    }
}
