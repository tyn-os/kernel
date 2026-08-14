//! Pure VFS file-descriptor table logic — no locks, no I/O, host-unit-testable
//! (tests/unit/).
//!
//! `vfs.rs` owns the `spin::Mutex<[Option<OpenFile>; MAX_OPEN]>` and the
//! monotonic fd counter; this owns the slot allocate / find / free logic over a
//! plain slice. The invariants worth guarding are the classic fd-table ones —
//! reuse after free, no use-after-free hit, exhaustion as a clean `None` (not an
//! out-of-bounds index), and out-of-range lookups missing cleanly. Verus-
//! tractable. See directions/PHASE2_LAYER1_FINISH.md.

/// An open VFS file — a position into the embedded cpio archive data.
pub struct OpenFile {
    pub fd: i32,
    pub data_offset: usize, // offset into cpio_data() where content starts
    pub data_len: usize,    // file size
    pub pos: usize,         // current read position
}

/// Place `entry` in the first free slot; returns its index, or `None` if the
/// table is full (the caller maps that to -EMFILE). Never indexes out of bounds.
pub fn alloc(table: &mut [Option<OpenFile>], entry: OpenFile) -> Option<usize> {
    for (i, slot) in table.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(entry);
            return Some(i);
        }
    }
    None
}

/// Index of the slot holding `fd`, or `None` if no open slot matches (unknown /
/// freed / out-of-range fd — a clean miss, never a stale or dangling hit).
pub fn find(table: &[Option<OpenFile>], fd: i32) -> Option<usize> {
    table
        .iter()
        .position(|slot| matches!(slot, Some(f) if f.fd == fd))
}

/// Free the slot holding `fd`. Returns `true` if a slot was freed, `false` if
/// `fd` was not open (so a double-free is a defined no-op, not corruption).
pub fn free(table: &mut [Option<OpenFile>], fd: i32) -> bool {
    match find(table, fd) {
        Some(i) => {
            table[i] = None;
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(fd: i32) -> OpenFile {
        OpenFile { fd, data_offset: 0, data_len: 0, pos: 0 }
    }

    #[test]
    fn alloc_find_free_reuse() {
        let mut t: [Option<OpenFile>; 3] = [None, None, None];
        let i = alloc(&mut t, of(1000)).unwrap();
        assert_eq!(find(&t, 1000), Some(i));
        assert!(free(&mut t, 1000));
        assert_eq!(find(&t, 1000), None); // use-after-free ⇒ clean miss
        // the freed slot is reusable (first-None), not leaked
        let j = alloc(&mut t, of(1001)).unwrap();
        assert_eq!(j, i);
    }

    #[test]
    fn double_free_is_a_defined_no_op() {
        let mut t: [Option<OpenFile>; 3] = [None, None, None];
        alloc(&mut t, of(1000)).unwrap();
        assert!(free(&mut t, 1000)); // first free succeeds
        assert!(!free(&mut t, 1000)); // second free ⇒ false, no panic/corruption
        assert!(t.iter().all(|s| s.is_none()));
    }

    #[test]
    fn exhaustion_returns_none_not_overflow() {
        let mut t: [Option<OpenFile>; 2] = [None, None];
        assert!(alloc(&mut t, of(1000)).is_some());
        assert!(alloc(&mut t, of(1001)).is_some());
        assert_eq!(alloc(&mut t, of(1002)), None); // full ⇒ None, not an OOB write
        // the two live slots are untouched
        assert_eq!(find(&t, 1000), Some(0));
        assert_eq!(find(&t, 1001), Some(1));
    }

    #[test]
    fn out_of_range_and_unknown_fd_miss_cleanly() {
        let mut t: [Option<OpenFile>; 3] = [None, None, None];
        alloc(&mut t, of(1000)).unwrap();
        assert_eq!(find(&t, 42), None); // never allocated
        assert_eq!(find(&t, -1), None); // negative fd — no OOB index
        assert_eq!(find(&t, i32::MAX), None);
        assert!(!free(&mut t, 999)); // free of an unknown fd ⇒ false
    }

    #[test]
    fn stdio_fds_are_not_in_the_vfs_table() {
        // VFS fds start at 1000 (NEXT_VFS_FD); 0/1/2 (stdin/stdout/stderr) are
        // handled by the syscall layer (serial), never allocated here — so a VFS
        // lookup of them is a clean miss, and never aliases a real open file.
        let mut t: [Option<OpenFile>; 3] = [None, None, None];
        alloc(&mut t, of(1000)).unwrap();
        assert_eq!(find(&t, 0), None);
        assert_eq!(find(&t, 1), None);
        assert_eq!(find(&t, 2), None);
    }
}
