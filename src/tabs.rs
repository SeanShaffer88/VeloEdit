//! Tab-strip bookkeeping (roadmap item 39, slice R5b).
//!
//! The window holds several documents in one workspace. The shell
//! keeps the ACTIVE document's state in its own fields (every
//! existing feature reads and writes those), and the inactive
//! documents' states in a "parked" list beside it. This module is
//! the pure index arithmetic over that arrangement — conceptual
//! tab order, which slot of the parked list a tab lives in, what
//! closing / moving / switching does to the active index — plus
//! the tab strip's width fitting and the session list's
//! serialisation maths. It is deliberately free of any window or
//! document type so the whole model is host-testable; the shell
//! (`gui_win`) owns the state swapping itself.
//!
//! The invariant the shell maintains, and every method here
//! assumes: the parked list holds `count - 1` states in conceptual
//! order with the active document left out. Parking the live state
//! inserts it at position `active`, which restores the full
//! conceptual list; un-parking removes the incoming document's
//! state from that same list.

/// The outcome of closing one tab, as the shell needs it: whether
/// the closed tab was the active one, and which index is active
/// afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CloseEffect {
    /// The closed tab was the active tab.
    pub was_active: bool,
    /// The active index after the close (the document that slid
    /// into the closed tab's place, or the new last tab).
    pub active: usize,
}

/// Conceptual tab order: how many tabs exist and which one is
/// active. Starts as a single active tab, never drops below one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabOrder {
    count: usize,
    active: usize,
}

impl Default for TabOrder {
    fn default() -> Self {
        Self::new()
    }
}

impl TabOrder {
    /// One tab, active.
    pub fn new() -> Self {
        TabOrder {
            count: 1,
            active: 0,
        }
    }

    /// Number of tabs (always at least 1).
    pub fn count(&self) -> usize {
        self.count
    }

    /// The active tab's conceptual index (always `< count`).
    pub fn active(&self) -> usize {
        self.active
    }

    /// The parked-list index of conceptual tab `index`: `None`
    /// for the active tab (its state is live, not parked) and for
    /// out-of-range indices. Tabs before the active one sit at
    /// their own index; tabs after it sit one lower, because the
    /// active document is missing from the list.
    pub fn parked_index(&self, index: usize) -> Option<usize> {
        if index >= self.count || index == self.active {
            None
        } else if index < self.active {
            Some(index)
        } else {
            Some(index - 1)
        }
    }

    /// Where the live state goes in the parked list when it is
    /// parked: its own conceptual index (the parked list around it
    /// is exactly the conceptual list with it missing).
    pub fn park_position(&self) -> usize {
        self.active
    }

    /// Make `index` the active tab. Returns false (and changes
    /// nothing) when `index` is out of range or already active.
    pub fn activate(&mut self, index: usize) -> bool {
        if index >= self.count || index == self.active {
            return false;
        }
        self.active = index;
        true
    }

    /// The index Ctrl+Tab moves to (wraps at the end).
    pub fn next(&self) -> usize {
        (self.active + 1) % self.count
    }

    /// The index Ctrl+Shift+Tab moves to (wraps at the start).
    pub fn prev(&self) -> usize {
        (self.active + self.count - 1) % self.count
    }

    /// A new tab was appended at the end; it becomes active.
    /// Returns its index. The shell parks the previously live
    /// state before calling this (the parked list gains it at the
    /// old active position, which is exactly where the conceptual
    /// order needs it).
    pub fn opened_appended(&mut self) -> usize {
        self.count += 1;
        self.active = self.count - 1;
        self.active
    }

    /// Reset to a single fresh tab: used when the last remaining
    /// tab is closed and replaced in place by a new empty
    /// document (the window never has zero documents).
    pub fn reset_single(&mut self) {
        self.count = 1;
        self.active = 0;
    }

    /// Close conceptual tab `index`. The caller has already
    /// removed the tab's state (from the parked list, or by
    /// discarding the live state); this only fixes the numbers.
    /// Closing the active tab activates the document that slid
    /// into its place — or the new last tab when the closed one
    /// was last. Closing the ONLY tab is refused by the model:
    /// the count stays 1 (the shell replaces its contents with a
    /// fresh document instead, see [`TabOrder::reset_single`]);
    /// the returned effect reports the tab as active either way.
    pub fn closed(&mut self, index: usize) -> CloseEffect {
        debug_assert!(index < self.count);
        let was_active = index == self.active;
        if self.count > 1 {
            self.count -= 1;
            if index < self.active {
                self.active -= 1;
            } else if was_active {
                self.active = index.min(self.count - 1);
            }
        }
        CloseEffect {
            was_active,
            active: self.active,
        }
    }

    /// Move conceptual tab `from` to position `to` (the parked
    /// list is reordered to match by the caller). The active
    /// marker follows whichever document was active: moving the
    /// active tab moves the marker with it; moving another tab
    /// across the marker shifts it by one.
    pub fn moved(&mut self, from: usize, to: usize) {
        if from >= self.count || to >= self.count || from == to {
            return;
        }
        if self.active == from {
            self.active = to;
        } else if from < self.active && to >= self.active {
            self.active -= 1;
        } else if from > self.active && to <= self.active {
            self.active += 1;
        }
    }
}

/// Fit tab widths into the strip: each tab wants its natural
/// width; when the total overflows `avail`, tabs shrink toward
/// `min` — the widest tabs give up space first (water-filling),
/// and no tab ends below `min` or above its natural width. When
/// even `min` for every tab overflows, every tab is `min` and the
/// strip scrolls. Returns one width per input, same order.
/// The natural width of one tab: the label's rendered width plus
/// the exact chrome the painter spends around it — left padding
/// (8), one slot per state marker (10 each), the gap before the
/// close box (2), the close box (18), and the right padding (2).
/// The painter advances exactly these amounts and clips the label
/// to what remains, so a budget built from anything less (the
/// field failure: an estimated label width and a smaller chrome
/// allowance) clips the label's last character.
pub fn natural_width(text_w: f32, markers: usize) -> f32 {
    8.0 + markers as f32 * 10.0 + text_w + 2.0 + 18.0 + 2.0
}

pub fn fit_widths(natural: &[f32], min: f32, avail: f32) -> Vec<f32> {
    let total: f32 = natural.iter().sum();
    if total <= avail || natural.is_empty() {
        return natural.to_vec();
    }
    let mut widths: Vec<f32> = natural.iter().map(|w| w.max(min)).collect();
    // Repeatedly shave the currently widest tabs down toward the
    // next level until the total fits or everything sits at min.
    // Widths are few (one per open tab), so the simple loop is fine.
    loop {
        let total: f32 = widths.iter().sum();
        if total <= avail {
            break;
        }
        let widest = widths.iter().copied().fold(f32::MIN, f32::max);
        if widest <= min {
            break; // everything is at the floor; the strip scrolls
        }
        // The level below the widest: the next-distinct width, or
        // the floor — shaving to it keeps the widest tabs level.
        let next_level = widths
            .iter()
            .copied()
            .filter(|w| *w < widest)
            .fold(min, f32::max);
        let sharing = widths.iter().filter(|w| **w >= widest).count() as f32;
        let excess = total - avail;
        let shave = ((widest - next_level) * sharing).min(excess) / sharing;
        for w in widths.iter_mut() {
            if *w >= widest {
                *w -= shave;
            }
        }
        if shave <= 0.0 {
            break;
        }
    }
    widths
}

/// The most tabs the persisted session list holds; a longer list
/// is truncated on save and on load (the window geometry of a
/// 33-tab session is not a case worth designing around).
pub const MAX_SESSION_TABS: usize = 32;

/// Reduce the window's documents to the persisted session list:
/// the paths of the documents that have one, in tab order, and
/// the active tab's index WITHIN that kept list. Unsaved
/// documents (no path) are not persisted — a dirty never-saved
/// document keeps the crash-recovery path instead. When the
/// active document has no path, the active index lands on the
/// nearest kept document at or before it (the first kept one when
/// there is none before).
pub fn session_from_docs(docs: &[Option<String>], active: usize) -> (Vec<String>, usize) {
    let mut paths: Vec<String> = Vec::new();
    let mut kept_active = 0usize;
    for (i, doc) in docs.iter().enumerate() {
        if let Some(path) = doc {
            if i <= active {
                kept_active = paths.len();
            }
            paths.push(path.clone());
        }
    }
    paths.truncate(MAX_SESSION_TABS);
    let active = if paths.is_empty() {
        0
    } else {
        kept_active.min(paths.len() - 1)
    };
    (paths, active)
}

/// Plan a session restore: which saved paths to open (the ones
/// the `exists` probe accepts, in saved order) and which of them
/// starts active — the saved active path when it survives, else
/// the nearest surviving path before it, else the first.
pub fn plan_restore(
    saved: &[String],
    saved_active: usize,
    mut exists: impl FnMut(&str) -> bool,
) -> (Vec<String>, usize) {
    let mut kept: Vec<String> = Vec::new();
    let mut active = 0usize;
    for (i, path) in saved.iter().enumerate() {
        if exists(path) {
            if i <= saved_active {
                active = kept.len();
            }
            kept.push(path.clone());
        }
    }
    (kept, active)
}

/// Normalise a session list coming from the settings file: drop
/// empty entries, cap the length, and clamp the active index.
pub fn normalise_session(paths: Vec<String>, active: usize) -> (Vec<String>, usize) {
    let mut paths: Vec<String> = paths.into_iter().filter(|p| !p.is_empty()).collect();
    paths.truncate(MAX_SESSION_TABS);
    let active = if paths.is_empty() {
        0
    } else {
        active.min(paths.len() - 1)
    };
    (paths, active)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(count: usize, active: usize) -> TabOrder {
        let mut order = TabOrder::new();
        for _ in 1..count {
            order.opened_appended();
        }
        // opened_appended leaves the last tab active; walk the
        // marker to the wanted one.
        order.active = active;
        order
    }

    #[test]
    fn natural_width_covers_the_painters_whole_chrome() {
        // The painter spends 8 (left pad) + 10 per marker + the
        // label + 2 (gap) + 18 (close box) + 2 (right pad); the
        // natural width must cover all of it or the label clips.
        assert_eq!(natural_width(60.0, 0), 90.0);
        assert_eq!(natural_width(60.0, 1), 100.0);
        assert_eq!(natural_width(60.0, 2), 110.0);
        assert_eq!(natural_width(0.0, 0), 30.0);
    }

    #[test]
    fn parked_index_skips_the_active_tab() {
        let order = order(4, 2);
        assert_eq!(order.parked_index(0), Some(0));
        assert_eq!(order.parked_index(1), Some(1));
        assert_eq!(order.parked_index(2), None, "the active tab is live");
        assert_eq!(order.parked_index(3), Some(2));
        assert_eq!(order.parked_index(4), None, "out of range");
        assert_eq!(order.park_position(), 2);
    }

    #[test]
    fn next_and_prev_wrap() {
        let mut order = order(3, 0);
        assert_eq!(order.next(), 1);
        assert_eq!(order.prev(), 2);
        order.activate(2);
        assert_eq!(order.next(), 0);
        assert_eq!(order.prev(), 1);
        let single = TabOrder::new();
        assert_eq!(single.next(), 0);
        assert_eq!(single.prev(), 0);
    }

    #[test]
    fn activate_rejects_out_of_range_and_self() {
        let mut order = order(3, 1);
        assert!(!order.activate(1));
        assert!(!order.activate(3));
        assert!(order.activate(0));
        assert_eq!(order.active(), 0);
    }

    #[test]
    fn opened_appended_counts_and_activates() {
        let mut order = TabOrder::new();
        assert_eq!(order.opened_appended(), 1);
        assert_eq!(order.count(), 2);
        assert_eq!(order.active(), 1);
        assert_eq!(order.opened_appended(), 2);
        assert_eq!(order.count(), 3);
    }

    #[test]
    fn closing_before_the_active_shifts_it_down() {
        let mut order = order(4, 3);
        let effect = order.closed(1);
        assert!(!effect.was_active);
        assert_eq!(effect.active, 2);
        assert_eq!(order.count(), 3);
        assert_eq!(order.active(), 2);
    }

    #[test]
    fn closing_after_the_active_keeps_it() {
        let mut order = order(4, 1);
        let effect = order.closed(3);
        assert!(!effect.was_active);
        assert_eq!(order.active(), 1);
        assert_eq!(order.count(), 3);
    }

    #[test]
    fn closing_the_active_activates_the_neighbour_that_slides_in() {
        let mut order = order(4, 1);
        let effect = order.closed(1);
        assert!(effect.was_active);
        // The old tab 2 now sits at index 1 and is active.
        assert_eq!(effect.active, 1);
        assert_eq!(order.count(), 3);
    }

    #[test]
    fn closing_the_last_tab_activates_the_new_last() {
        let mut order = order(3, 2);
        let effect = order.closed(2);
        assert!(effect.was_active);
        assert_eq!(effect.active, 1);
        assert_eq!(order.count(), 2);
    }

    #[test]
    fn closing_the_only_tab_keeps_the_single_tab_shape() {
        let mut order = TabOrder::new();
        let effect = order.closed(0);
        assert!(effect.was_active);
        assert_eq!(order.count(), 1);
        assert_eq!(order.active(), 0);
        order.reset_single();
        assert_eq!(order.count(), 1);
        assert_eq!(order.active(), 0);
    }

    #[test]
    fn moving_a_tab_carries_the_active_marker_with_its_document() {
        let mut order = order(5, 2);
        order.moved(2, 4);
        assert_eq!(order.active(), 4);
        order.moved(4, 0);
        assert_eq!(order.active(), 0);
        assert_eq!(order.count(), 5);
    }

    #[test]
    fn moving_another_tab_across_the_active_shifts_the_marker() {
        let mut order = order(5, 2);
        order.moved(0, 3);
        assert_eq!(order.active(), 1, "a tab before moved past it");
        order.moved(4, 0);
        assert_eq!(order.active(), 2, "a tab after moved before it");
        order.moved(0, 1);
        assert_eq!(order.active(), 2, "a move not crossing it changes nothing");
        order.moved(3, 4);
        assert_eq!(order.active(), 2);
    }

    #[test]
    fn fit_widths_keeps_natural_widths_when_they_fit() {
        assert_eq!(fit_widths(&[100.0, 120.0], 60.0, 300.0), vec![100.0, 120.0]);
        assert!(fit_widths(&[], 60.0, 300.0).is_empty());
    }

    #[test]
    fn fit_widths_shrinks_the_widest_first() {
        // 200 + 100 into 240: the wide tab gives up 60, the
        // narrow one keeps its natural width.
        let widths = fit_widths(&[200.0, 100.0], 60.0, 240.0);
        assert_eq!(widths, vec![140.0, 100.0]);
        // Equal tabs share the shrink evenly.
        let widths = fit_widths(&[150.0, 150.0, 150.0], 60.0, 300.0);
        assert_eq!(widths, vec![100.0, 100.0, 100.0]);
    }

    #[test]
    fn fit_widths_never_goes_below_the_minimum() {
        let widths = fit_widths(&[200.0, 200.0], 76.0, 100.0);
        assert_eq!(widths, vec![76.0, 76.0], "the strip scrolls instead");
        // A natural width below the minimum is raised to it.
        let widths = fit_widths(&[40.0, 200.0], 76.0, 200.0);
        assert!(widths.iter().all(|w| *w >= 76.0));
        assert!((widths.iter().sum::<f32>() - 200.0).abs() < 0.01);
    }

    #[test]
    fn session_from_docs_keeps_paths_and_maps_the_active_index() {
        let docs = vec![
            Some("a.txt".to_string()),
            None,
            Some("b.txt".to_string()),
            Some("c.txt".to_string()),
        ];
        // Active on a kept document: its position in the list.
        let (paths, active) = session_from_docs(&docs, 2);
        assert_eq!(paths, vec!["a.txt", "b.txt", "c.txt"]);
        assert_eq!(active, 1);
        // Active on an unsaved document: the nearest kept one
        // at or before it.
        let (_, active) = session_from_docs(&docs, 1);
        assert_eq!(active, 0);
        // Active before every kept document.
        let docs = vec![None, Some("x.txt".to_string())];
        let (paths, active) = session_from_docs(&docs, 0);
        assert_eq!(paths, vec!["x.txt"]);
        assert_eq!(active, 0);
        // Nothing to persist.
        let (paths, active) = session_from_docs(&[None, None], 1);
        assert!(paths.is_empty());
        assert_eq!(active, 0);
    }

    #[test]
    fn plan_restore_filters_missing_and_maps_active() {
        let saved = vec![
            "gone.txt".to_string(),
            "a.txt".to_string(),
            "also-gone.txt".to_string(),
            "b.txt".to_string(),
        ];
        let exists = |p: &str| p == "a.txt" || p == "b.txt";
        // The saved active path survives.
        let (kept, active) = plan_restore(&saved, 3, exists);
        assert_eq!(kept, vec!["a.txt", "b.txt"]);
        assert_eq!(active, 1);
        // The saved active path is gone: the survivor before it.
        let (kept, active) = plan_restore(&saved, 2, exists);
        assert_eq!(kept, vec!["a.txt", "b.txt"]);
        assert_eq!(active, 0);
        // Nothing survives.
        let (kept, active) = plan_restore(&saved, 1, |_| false);
        assert!(kept.is_empty());
        assert_eq!(active, 0);
    }

    #[test]
    fn normalise_session_drops_empties_caps_and_clamps() {
        let (paths, active) =
            normalise_session(vec!["".to_string(), "a".to_string(), "b".to_string()], 9);
        assert_eq!(paths, vec!["a", "b"]);
        assert_eq!(active, 1);
        let many: Vec<String> = (0..40).map(|i| format!("f{i}")).collect();
        let (paths, active) = normalise_session(many, 39);
        assert_eq!(paths.len(), MAX_SESSION_TABS);
        assert_eq!(active, MAX_SESSION_TABS - 1);
        let (paths, active) = normalise_session(Vec::new(), 3);
        assert!(paths.is_empty());
        assert_eq!(active, 0);
    }
}
