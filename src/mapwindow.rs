//! Window maths for mapped reads (slice R4c, roadmap item 1,
//! "mapping windows").
//!
//! A whole-file mapping is the default and stays the default.
//! When the owner sets `map_window_mb` in `data/settings.json`,
//! reads of a file larger than the window go through remapped
//! views instead: each read maps one window of the file at a
//! time. The Win32 side lives in [`crate::mmap_win`]
//! (`WindowedMap`); this module is the pure half — which window a
//! read lands in, how the view offset aligns, and how the view
//! length clamps at the end of the file — so the arithmetic is
//! covered by the host test suite like every other core.
//!
//! Two rules shape the maths:
//!
//! * `MapViewOfFile` requires the view offset to be a multiple
//!   of the system allocation granularity (64 KB on every Windows
//!   architecture VeloEdit ships for), so a read's view starts at
//!   the read offset rounded DOWN to that granularity, and the
//!   read itself starts `skip` bytes into the view.
//! * A view may run past the nominal window by at most the
//!   alignment slack (`window + alignment`), so one window always
//!   covers at least `window` fresh bytes from its aligned start —
//!   a caller walking forward never remaps for a handful of bytes.

/// The view-offset alignment: the Windows allocation granularity,
/// 64 KB on x86-64 and ARM64 alike.
pub const WINDOW_ALIGNMENT: u64 = 65_536;

/// One planned view: map `view_len` bytes at `view_offset`, then
/// read from `skip` bytes into the view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowPlan {
    /// File offset the view starts at (a multiple of the
    /// alignment).
    pub view_offset: u64,
    /// Bytes from the view start to the requested read offset.
    pub skip: usize,
    /// Bytes the view covers (clamped at the end of the file).
    pub view_len: u64,
}

impl WindowPlan {
    /// Bytes readable from this view starting at the read offset:
    /// `view_len - skip` (what remains of the window).
    pub fn readable(&self) -> u64 {
        self.view_len - self.skip as u64
    }
}

/// The window size in bytes for a `map_window_mb` setting value.
pub fn window_bytes(map_window_mb: u32) -> u64 {
    (map_window_mb as u64) << 20
}

/// Plan the view for reading `want` bytes at `offset` in a file
/// of `file_len` bytes, with views of at most `window` bytes
/// (plus alignment slack) aligned to `alignment`.
///
/// Returns `None` when there is nothing to plan: an empty file, a
/// read at or past the end, a zero-length read, or windowing
/// disabled (`window == 0` — the caller uses the whole-file path
/// instead, which stays byte-identical to the pre-window
/// behaviour).
pub fn plan_window(
    file_len: u64,
    offset: u64,
    want: u64,
    window: u64,
    alignment: u64,
) -> Option<WindowPlan> {
    if file_len == 0 || offset >= file_len || want == 0 || window == 0 || alignment == 0 {
        return None;
    }
    let view_offset = offset - (offset % alignment);
    let skip = (offset - view_offset) as usize;
    // The view covers the window plus the alignment slack the
    // read offset consumed, clamped at the end of the file.
    let view_len = (window + skip as u64).min(file_len - view_offset);
    Some(WindowPlan {
        view_offset,
        skip,
        view_len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1024 * 1024;

    #[test]
    fn window_bytes_matches_the_setting_units() {
        assert_eq!(window_bytes(0), 0);
        assert_eq!(window_bytes(512), 512 * MB);
        assert_eq!(window_bytes(4096), 4096 * MB);
    }

    #[test]
    fn an_aligned_read_starts_its_own_view() {
        let plan = plan_window(10 * MB, 3 * MB, 100, MB, WINDOW_ALIGNMENT).unwrap();
        assert_eq!(plan.view_offset, 3 * MB); // 3 MB is 64 KB-aligned
        assert_eq!(plan.skip, 0);
        assert_eq!(plan.view_len, MB);
        assert_eq!(plan.readable(), MB);
    }

    #[test]
    fn an_unaligned_read_aligns_the_view_down() {
        let offset = 70_000;
        let plan = plan_window(10 * MB, offset, 100, MB, WINDOW_ALIGNMENT).unwrap();
        assert_eq!(plan.view_offset, 65_536);
        assert_eq!(plan.view_offset % WINDOW_ALIGNMENT, 0);
        assert_eq!(plan.skip, (offset - 65_536) as usize);
        // The view runs a full window past the read offset despite
        // the slack the alignment consumed.
        assert!(plan.readable() >= MB);
        assert_eq!(plan.view_offset + plan.view_len, 65_536 + MB + 4464);
    }

    #[test]
    fn the_view_clamps_at_the_end_of_the_file() {
        let file_len = 3 * MB + 123;
        let plan = plan_window(file_len, 3 * MB, MB, MB, WINDOW_ALIGNMENT).unwrap();
        assert_eq!(plan.view_len, 123);
        assert_eq!(plan.readable(), 123);
        // A read starting inside the last partial page still works.
        let plan = plan_window(file_len, file_len - 1, 10, MB, WINDOW_ALIGNMENT).unwrap();
        assert_eq!(plan.view_offset + plan.view_len, file_len);
        assert_eq!(plan.readable(), 1);
    }

    #[test]
    fn degenerate_reads_plan_nothing() {
        assert_eq!(plan_window(0, 0, 10, MB, WINDOW_ALIGNMENT), None);
        assert_eq!(plan_window(100, 100, 10, MB, WINDOW_ALIGNMENT), None);
        assert_eq!(plan_window(100, 0, 0, MB, WINDOW_ALIGNMENT), None);
        // Windowing off → no plan; the caller maps the whole file.
        assert_eq!(plan_window(100, 0, 10, 0, WINDOW_ALIGNMENT), None);
        assert_eq!(plan_window(100, 0, 10, MB, 0), None);
    }

    #[test]
    fn walking_forward_covers_the_file_without_gaps() {
        // Simulate a forward walk in odd-sized steps: consecutive
        // plans must always offer at least one fresh byte and the
        // walk must terminate exactly at the file end.
        let file_len = 5 * MB + 999;
        let mut pos = 0u64;
        let mut steps = 0u32;
        while pos < file_len {
            let plan = plan_window(file_len, pos, 100_000, MB, WINDOW_ALIGNMENT)
                .expect("a plan inside the file");
            assert_eq!(plan.view_offset % WINDOW_ALIGNMENT, 0);
            assert!(plan.view_offset <= pos);
            assert!(plan.readable() > 0);
            pos += plan.readable().min(100_000);
            steps += 1;
            assert!(steps < 10_000, "the walk must terminate");
        }
        assert_eq!(pos, file_len);
    }
}
