//! Pointer-following core (roadmap R4): stand on a byte offset in
//! a binary file, read the value stored there as a 32- or 64-bit
//! integer in either byte order, and learn which readings land
//! inside the document — the plausible jump targets for structures
//! that store offsets (headers, indexes, chunk tables). The shell
//! offers the in-range targets as one-click jumps and records them
//! in a [`JumpHistory`].
//!
//! The history is a pure value type in core on purpose: the
//! navigation *rule* (push on jump, back/forward, cap, no
//! duplicate consecutive entries) is testable without a window,
//! while the *policy* of when to push and when to clear (on open,
//! on jump, never on plain scrolling) belongs to the shell, which
//! owns the history instance and clears it when a document opens.

/// One integer reading of the bytes at an offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerValue {
    /// The value read.
    pub value: u64,
    /// True when the value is a byte position inside the document
    /// (`value < doc_len`; the document length itself is the
    /// end-of-file, not a position in it).
    pub in_document: bool,
}

/// The four readings at an offset. A reading is `None` when the
/// slice does not hold enough bytes for it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PointerTargets {
    pub u32_le: Option<PointerValue>,
    pub u32_be: Option<PointerValue>,
    pub u64_le: Option<PointerValue>,
    pub u64_be: Option<PointerValue>,
}

impl PointerTargets {
    /// The readings that land inside the document, in field order
    /// (u32 LE, u32 BE, u64 LE, u64 BE) — the shell's jump menu.
    pub fn in_document(&self) -> Vec<PointerValue> {
        [self.u32_le, self.u32_be, self.u64_le, self.u64_be]
            .into_iter()
            .flatten()
            .filter(|p| p.in_document)
            .collect()
    }
}

/// Read the pointer interpretations of `bytes` — the bytes at the
/// offset of interest (as many as the caller could read; fewer
/// than 8 simply yields fewer readings) — against a document of
/// `doc_len` bytes.
pub fn pointer_targets(bytes: &[u8], doc_len: u64) -> PointerTargets {
    let read = |value: u64| -> PointerValue {
        PointerValue {
            value,
            in_document: value < doc_len,
        }
    };
    PointerTargets {
        u32_le: bytes.get(..4).map(|b| {
            read(u64::from(u32::from_le_bytes(
                b.try_into().expect("4 bytes"),
            )))
        }),
        u32_be: bytes.get(..4).map(|b| {
            read(u64::from(u32::from_be_bytes(
                b.try_into().expect("4 bytes"),
            )))
        }),
        u64_le: bytes
            .get(..8)
            .map(|b| read(u64::from_le_bytes(b.try_into().expect("8 bytes")))),
        u64_be: bytes
            .get(..8)
            .map(|b| read(u64::from_be_bytes(b.try_into().expect("8 bytes")))),
    }
}

/// The jump-history cap: at most this many positions are kept.
pub const JUMP_HISTORY_CAP: usize = 128;

/// A back/forward stack of byte offsets. Pushing after going back
/// drops the forward entries (the usual browser rule); pushing the
/// current position again is a no-op; the oldest entries fall off
/// past [`JUMP_HISTORY_CAP`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JumpHistory {
    entries: Vec<u64>,
    /// Index of the current position; `entries.len()` when empty.
    cursor: usize,
}

impl JumpHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a jump to `offset`: forward entries are dropped,
    /// the position becomes current. Pushing the current position
    /// again changes nothing.
    pub fn push(&mut self, offset: u64) {
        if self.current() == Some(offset) {
            return;
        }
        let keep = if self.entries.is_empty() {
            0
        } else {
            self.cursor + 1
        };
        self.entries.truncate(keep);
        self.entries.push(offset);
        // Enforce the cap from the front (oldest first).
        if self.entries.len() > JUMP_HISTORY_CAP {
            let excess = self.entries.len() - JUMP_HISTORY_CAP;
            self.entries.drain(..excess);
        }
        self.cursor = self.entries.len() - 1;
    }

    /// The current position, if any.
    pub fn current(&self) -> Option<u64> {
        self.entries.get(self.cursor).copied()
    }

    /// Step back; returns the position landed on, or `None` when
    /// already at the oldest entry.
    pub fn back(&mut self) -> Option<u64> {
        if self.cursor > 0 && !self.entries.is_empty() {
            self.cursor -= 1;
            return self.current();
        }
        None
    }

    /// Step forward; returns the position landed on, or `None`
    /// when already at the newest entry.
    pub fn forward(&mut self) -> Option<u64> {
        if self.cursor + 1 < self.entries.len() {
            self.cursor += 1;
            return self.current();
        }
        None
    }

    pub fn can_back(&self) -> bool {
        self.cursor > 0 && !self.entries.is_empty()
    }

    pub fn can_forward(&self) -> bool {
        self.cursor + 1 < self.entries.len()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Forget everything (the shell calls this when a document
    /// opens).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.cursor = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readings_in_both_orders_and_widths() {
        // Bytes chosen so each reading differs: 01 00 00 00 | 00 00 00 02.
        let bytes = [0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02];
        let t = pointer_targets(&bytes, 16);
        assert_eq!(t.u32_le.unwrap().value, 1);
        assert_eq!(t.u32_be.unwrap().value, 0x0100_0000);
        assert_eq!(t.u64_le.unwrap().value, 0x0200_0000_0000_0001);
        assert_eq!(t.u64_be.unwrap().value, 0x0100_0000_0000_0002);
        // In a 16-byte document only the value 1 is a position.
        let inside = t.in_document();
        assert_eq!(inside.len(), 1);
        assert_eq!(inside[0].value, 1);
        // The document length itself is end-of-file, not inside.
        let t = pointer_targets(&16u32.to_le_bytes(), 16);
        assert!(!t.u32_le.unwrap().in_document);
        // Short slices yield fewer readings, never a panic.
        let t = pointer_targets(&[0x05, 0x00], 100);
        assert_eq!(t.u32_le, None);
        assert_eq!(t.u64_le, None);
        let t = pointer_targets(&[], 100);
        assert_eq!(t, PointerTargets::default());
    }

    #[test]
    fn history_push_back_forward_and_truncation() {
        let mut h = JumpHistory::new();
        assert_eq!(h.current(), None);
        assert_eq!(h.back(), None);
        h.push(100);
        h.push(200);
        h.push(300);
        assert_eq!(h.current(), Some(300));
        assert_eq!(h.back(), Some(200));
        assert_eq!(h.back(), Some(100));
        assert_eq!(h.back(), None);
        assert_eq!(h.forward(), Some(200));
        // A jump from mid-history drops the forward entries.
        h.push(999);
        assert_eq!(h.current(), Some(999));
        assert_eq!(h.forward(), None);
        assert!(!h.can_forward());
        assert_eq!(h.back(), Some(200));
        // Pushing the current position is a no-op.
        let len = h.len();
        h.push(200);
        assert_eq!(h.len(), len);
        // Clear forgets everything.
        h.clear();
        assert!(h.is_empty());
        assert_eq!(h.current(), None);
    }

    #[test]
    fn history_is_capped_oldest_first() {
        let mut h = JumpHistory::new();
        for i in 0..(JUMP_HISTORY_CAP as u64 + 10) {
            h.push(i * 2);
        }
        assert_eq!(h.len(), JUMP_HISTORY_CAP);
        assert_eq!(h.current(), Some((JUMP_HISTORY_CAP as u64 + 9) * 2));
        // The oldest ten fell off: stepping back to the start lands
        // on entry 10's value.
        while h.back().is_some() {}
        assert_eq!(h.current(), Some(20));
    }
}
