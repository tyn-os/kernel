//! Pure ENA submission/completion ring index + phase arithmetic — no MMIO,
//! host-unit-testable (tests/unit/).
//!
//! `device.rs` owns the volatile descriptor reads/writes and doorbells; this owns
//! the cursor math that decides which descriptor slot, when the phase bit
//! inverts, and whether the submission ring is full. Wraparound and phase-tracking
//! bugs are a classic source of *intermittent* RX/TX stalls (a stale phase read as
//! fresh, or a full ring mistaken for empty), so the edges are tested exhaustively
//! here. Verus-tractable. See directions/PHASE2_LAYER1_UNIT.md.

/// Descriptor slot for a free-running submission-queue index. `mask` is `depth-1`
/// (`depth` is a power of two).
#[inline]
pub fn slot(idx: u16, mask: u16) -> u16 {
    idx & mask
}

/// Advance a submission-queue cursor: a free-running u16 index plus a phase bit.
/// The device distinguishes freshly-posted descriptors by the phase bit written
/// into each; it inverts every time the index crosses a depth boundary
/// (`idx & mask == 0` after the increment).
#[inline]
pub fn sq_advance(idx: u16, phase: u8, mask: u16) -> (u16, u8) {
    let idx = idx.wrapping_add(1);
    let phase = if idx & mask == 0 { phase ^ 1 } else { phase };
    (idx, phase)
}

/// Advance a completion-queue cursor: a head bounded to `[0, depth)` plus a phase
/// bit that inverts on wrap. (The device writes completions with a phase bit; the
/// driver flips its expected phase each time the head wraps.)
#[inline]
pub fn cq_advance(head: u16, phase: u8, depth: u16) -> (u16, u8) {
    let head = head + 1;
    if head == depth {
        (0, phase ^ 1)
    } else {
        (head, phase)
    }
}

/// Is a completion descriptor's phase bit the one this cursor currently expects?
/// A mismatch means "the device hasn't written this slot yet" — stop draining.
#[inline]
pub fn entry_ready(desc_phase: u8, expected_phase: u8) -> bool {
    (desc_phase & 1) == (expected_phase & 1)
}

/// Free submission slots, given the posted (`tail`) and completed (`next_to_comp`)
/// free-running indices. One slot is always kept empty so *full* and *empty* are
/// distinguishable (equal indices ⇒ empty, never full); `wrapping_sub` keeps the
/// outstanding count correct across the u16 wrap.
#[inline]
pub fn free_slots(tail: u16, next_to_comp: u16, depth: u16) -> u16 {
    depth - 1 - tail.wrapping_sub(next_to_comp)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEPTH: u16 = 256;
    const MASK: u16 = DEPTH - 1;

    #[test]
    fn slot_wraps_at_capacity() {
        assert_eq!(slot(0, MASK), 0);
        assert_eq!(slot(255, MASK), 255);
        assert_eq!(slot(256, MASK), 0); // wrapped
        assert_eq!(slot(257, MASK), 1);
        assert_eq!(slot(0xFFFF, MASK), 255);
    }

    #[test]
    fn sq_phase_inverts_exactly_at_the_boundary() {
        // Cross the depth boundary from the last slot: phase must flip.
        let (idx, ph) = sq_advance(255, 1, MASK);
        assert_eq!((idx, ph), (256, 0)); // idx & MASK == 0 -> flip
        // A mid-ring advance must NOT flip.
        let (idx, ph) = sq_advance(5, 1, MASK);
        assert_eq!((idx, ph), (6, 1));
    }

    #[test]
    fn sq_phase_period_is_two_full_laps() {
        let (mut idx, mut ph) = (0u16, 1u8);
        for _ in 0..DEPTH {
            let (i, p) = sq_advance(idx, ph, MASK);
            idx = i;
            ph = p;
        }
        assert_eq!((idx, ph), (256, 0)); // one lap -> phase inverted
        for _ in 0..DEPTH {
            let (i, p) = sq_advance(idx, ph, MASK);
            idx = i;
            ph = p;
        }
        assert_eq!((idx, ph), (512, 1)); // two laps -> phase back
    }

    #[test]
    fn sq_index_wraps_u16_cleanly_since_depth_divides_65536() {
        // Advancing 65536 times returns to (0, start-phase): 65536 / 256 = 256
        // laps (even), so phase is back to start and the index wraps to 0.
        let (mut idx, mut ph) = (0u16, 1u8);
        for _ in 0..65536u32 {
            let (i, p) = sq_advance(idx, ph, MASK);
            idx = i;
            ph = p;
        }
        assert_eq!((idx, ph), (0, 1));
    }

    #[test]
    fn cq_advance_wraps_and_inverts_phase() {
        assert_eq!(cq_advance(0, 1, DEPTH), (1, 1));
        assert_eq!(cq_advance(DEPTH - 1, 1, DEPTH), (0, 0)); // wrap -> phase flip
        assert_eq!(cq_advance(DEPTH - 1, 0, DEPTH), (0, 1));
    }

    #[test]
    fn entry_ready_is_phase_equality() {
        assert!(entry_ready(1, 1));
        assert!(entry_ready(0, 0));
        assert!(!entry_ready(1, 0)); // stale slot, not yet written
        assert!(!entry_ready(0, 1));
        // only bit 0 matters
        assert!(entry_ready(0xFE, 0));
        assert!(entry_ready(0xFF, 1));
    }

    #[test]
    fn free_slots_empty_full_and_the_kept_slot() {
        // Equal indices ⇒ empty ⇒ depth-1 free (index equality means empty here).
        assert_eq!(free_slots(0, 0, DEPTH), DEPTH - 1);
        assert_eq!(free_slots(100, 100, DEPTH), DEPTH - 1);
        // One posted, none completed ⇒ one fewer free.
        assert_eq!(free_slots(1, 0, DEPTH), DEPTH - 2);
        // Full: depth-1 outstanding ⇒ 0 free (the kept slot).
        assert_eq!(free_slots(DEPTH - 1, 0, DEPTH), 0);
    }

    #[test]
    fn free_slots_correct_across_u16_wrap() {
        // tail has wrapped past next_to_comp: outstanding = wrapping_sub.
        // tail=3, ntc=0xFFFD -> outstanding = 3 - 0xFFFD (mod 2^16) = 6.
        assert_eq!(free_slots(3, 0xFFFD, DEPTH), DEPTH - 1 - 6);
        // both near the top, tail just ahead
        assert_eq!(free_slots(0xFFFF, 0xFFFE, DEPTH), DEPTH - 2);
        // wrap exactly at the u16 boundary
        assert_eq!(free_slots(0, 0xFFFF, DEPTH), DEPTH - 2);
    }
}
