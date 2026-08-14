//! Pure tmpfs path + membership + byte-cap logic — no locks, no raw pointers, no
//! kernel deps. Host-unit-testable (tests/unit/).
//!
//! `tmpfs.rs` owns the `spin::Mutex<Option<Tmpfs>>`, the `BTreeMap` node store,
//! the raw user-buffer copies, and the fd table. This owns the parts with
//! off-by-one teeth and prefix traps: path normalization, the parent walk, the
//! mount-routing predicate, the descendant (delete-non-empty) predicate, and —
//! the headline — the write byte-cap arithmetic. See
//! directions/PHASE2_LAYER1_FINISH.md and tests/unit/COVERAGE.md for what stays
//! an in-situ seam (the node-tree create/delete decisions are NOT extracted;
//! they'd be a copy, not a guard).

// `Vec` source is target-aware so this same file compiles both in the no_std
// kernel (alloc, via lib.rs's crate-root `extern crate alloc`) and in the std
// host unit-test crate. An `extern crate alloc` here would duplicate alloc's
// lang items under std — hence the cfg split, not an unconditional import.
#[cfg(not(test))]
use alloc::vec::Vec;
#[cfg(test)]
use std::vec::Vec;

/// Normalise a path: ensure leading `/`, strip a leading `./`, and drop any
/// trailing slash (except for root). Does NOT resolve `..`/`.` beyond a leading
/// `./` — the callers we serve (Elixir/ERTS temp paths) pass already-clean
/// absolute paths.
pub fn norm(path: &[u8]) -> Vec<u8> {
    let mut p = path;
    if p.starts_with(b"./") {
        p = &p[1..]; // "./tmp/x" -> "/tmp/x"
    }
    let mut v: Vec<u8> = if p.starts_with(b"/") {
        p.to_vec()
    } else {
        let mut x = Vec::with_capacity(p.len() + 1);
        x.push(b'/');
        x.extend_from_slice(p);
        x
    };
    while v.len() > 1 && *v.last().unwrap() == b'/' {
        v.pop();
    }
    v
}

/// The parent directory path of a normalised path. `/tmp/a/b` -> `/tmp`,
/// `/tmp` -> `/` (never a tmpfs node, so creating directly in a mount root works
/// but creating in `/` does not).
pub fn parent(path: &[u8]) -> Vec<u8> {
    match path.iter().rposition(|&c| c == b'/') {
        Some(0) => b"/".to_vec(),
        Some(i) => path[..i].to_vec(),
        None => b"/".to_vec(),
    }
}

/// Mount-routing predicate over an already-normalised path: does tmpfs own it?
/// The two mounts are `/tmp` and `/dev/shm` (and their subtrees). The prefix
/// trap this guards: `/tmpx` is NOT under `/tmp` — membership is the exact mount
/// or the mount followed by `/`, never a bare string prefix.
pub fn is_mount_path(n: &[u8]) -> bool {
    n == b"/tmp" || n.starts_with(b"/tmp/") || n == b"/dev/shm" || n.starts_with(b"/dev/shm/")
}

/// Is `key` a (possibly deep) descendant of directory `dir`? This is the
/// delete-non-empty gate: rmdir/rename refuse a directory for which any key is
/// under it. The prefix trap: `/tmpx` is not under `/tmp`; a dir is not its own
/// child. Membership is `dir` + `/` as a prefix.
pub fn is_under(key: &[u8], dir: &[u8]) -> bool {
    // key is under dir iff it begins with `dir` then a `/` then at least one byte.
    key.len() > dir.len() && key.starts_with(dir) && key[dir.len()] == b'/'
}

/// Byte-cap arithmetic for a write of `count` bytes at absolute offset `at` into
/// a file of current length `len`, when the filesystem holds `total` of `cap`
/// bytes. Returns the number of bytes the write may place — `count` if it all
/// fits, a smaller partial count if the cap truncates it, or `0` (which the
/// caller maps to ENOSPC when `count > 0`).
///
/// Growth needed to write `n` bytes at `at` is `grow(n) = max(0, at + n - len)`:
/// it includes any zero-filled GAP when `at > len` (a sparse write past EOF),
/// not just the payload. Bytes overwriting existing content (`at < len`) are
/// FREE — they don't grow the file, so they're granted even at `total == cap`.
/// This solves `grow(n) <= (cap - total)` for the largest `n <= count`.
pub fn grant_write(total: usize, cap: usize, at: usize, len: usize, count: usize) -> usize {
    let allowed_grow = cap.saturating_sub(total);
    let max_n = if at >= len {
        // gap (at - len) plus payload must fit; if the gap alone overflows, 0.
        allowed_grow.saturating_sub(at - len)
    } else {
        (len - at) + allowed_grow // free overwrite region + growable tail
    };
    count.min(max_n)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- norm -------------------------------------------------------------
    #[test]
    fn norm_adds_leading_slash_and_strips_dot_slash() {
        assert_eq!(norm(b"tmp/x"), b"/tmp/x");
        assert_eq!(norm(b"./tmp/x"), b"/tmp/x"); // "./" -> "/"
        assert_eq!(norm(b"/tmp/x"), b"/tmp/x"); // already absolute, unchanged
    }

    #[test]
    fn norm_drops_trailing_slash_but_keeps_root() {
        assert_eq!(norm(b"/tmp/"), b"/tmp");
        assert_eq!(norm(b"/tmp///"), b"/tmp"); // collapses a run of trailing '/'
        assert_eq!(norm(b"/"), b"/"); // root is NOT emptied
        assert_eq!(norm(b"//"), b"/"); // degenerates to root, not ""
        assert_eq!(norm(b""), b"/"); // empty -> root, never a bare ""
    }

    #[test]
    fn norm_is_idempotent() {
        for p in [&b"tmp/x"[..], b"./tmp/x", b"/tmp/", b"/", b""] {
            let once = norm(p);
            assert_eq!(norm(&once), once, "norm not idempotent on {:?}", p);
        }
    }

    // ---- parent -----------------------------------------------------------
    #[test]
    fn parent_walks_one_level() {
        assert_eq!(parent(b"/tmp/a/b"), b"/tmp/a");
        assert_eq!(parent(b"/tmp/a"), b"/tmp");
        assert_eq!(parent(b"/tmp"), b"/"); // mount root's parent is /
        assert_eq!(parent(b"/"), b"/"); // root's parent is itself, not underflow
    }

    // ---- is_mount_path (routing) ------------------------------------------
    #[test]
    fn mount_membership_is_exact_or_slash_prefixed() {
        assert!(is_mount_path(b"/tmp"));
        assert!(is_mount_path(b"/tmp/foo"));
        assert!(is_mount_path(b"/dev/shm"));
        assert!(is_mount_path(b"/dev/shm/foo"));
    }

    #[test]
    fn mount_membership_rejects_the_prefix_trap() {
        assert!(!is_mount_path(b"/tmpx")); // NOT under /tmp — the teeth
        assert!(!is_mount_path(b"/tmp2/y"));
        assert!(!is_mount_path(b"/dev/shmx")); // NOT under /dev/shm
        assert!(!is_mount_path(b"/dev/sh")); // shorter, unrelated
        assert!(!is_mount_path(b"/")); // root is not writable
        assert!(!is_mount_path(b"/home/x")); // outside any mount
    }

    // ---- is_under (delete-non-empty gate) ---------------------------------
    #[test]
    fn descendant_predicate_matches_children_and_grandchildren() {
        assert!(is_under(b"/tmp/a", b"/tmp")); // direct child
        assert!(is_under(b"/tmp/a/b", b"/tmp")); // deep descendant still blocks rmdir
        assert!(is_under(b"/tmp/a/b", b"/tmp/a"));
    }

    #[test]
    fn descendant_predicate_rejects_prefix_trap_and_self() {
        assert!(!is_under(b"/tmpx", b"/tmp")); // prefix but not a child — the teeth
        assert!(!is_under(b"/tmp", b"/tmp")); // a dir is not its own child
        assert!(!is_under(b"/tmp", b"/tmp/a")); // parent is not under its child
        assert!(!is_under(b"/other/a", b"/tmp"));
    }

    // ---- grant_write (the 4 MiB cap off-by-one) ---------------------------
    #[test]
    fn grant_fits_below_cap() {
        // empty fs, plenty of room: the whole write is granted.
        assert_eq!(grant_write(0, 10, 0, 0, 5), 5);
    }

    #[test]
    fn grant_exact_fill_reaches_the_cap_boundary() {
        // total=9 of cap=10, appending at EOF(len=9): exactly 1 byte fits (cap-1
        // fits, the write lands ON the boundary). This is the cap / off-by-one.
        assert_eq!(grant_write(9, 10, 9, 9, 5), 1);
        // and writing that 1 byte would bring total to exactly cap — allowed.
    }

    #[test]
    fn grant_at_cap_returns_zero_not_one_over() {
        // total==cap, appending: nothing fits -> 0 (caller: ENOSPC). The cap+1 case.
        assert_eq!(grant_write(10, 10, 10, 10, 1), 0);
    }

    #[test]
    fn grant_sparse_gap_spends_budget() {
        // empty file, write at offset 8 (a hole): the 8-byte zero-fill gap counts
        // against the cap, so only cap-gap = 2 payload bytes fit, not 5.
        assert_eq!(grant_write(0, 10, 8, 0, 5), 2);
    }

    #[test]
    fn grant_gap_alone_overflowing_is_zero_not_underflow() {
        // gap (15) already exceeds the budget (10): saturating, so 0 — never a
        // wrapped-huge grant.
        assert_eq!(grant_write(0, 10, 15, 0, 5), 0);
    }

    #[test]
    fn grant_overwrite_is_free_even_at_full_cap() {
        // total==cap==10, file is 10 bytes, overwrite 4 bytes in place at at=0:
        // no growth needed, so it's granted despite the fs being full. The teeth:
        // a naive `cap - total` gate (=0) would wrongly refuse a pure overwrite.
        assert_eq!(grant_write(10, 10, 0, 10, 4), 4);
        // partial overwrite that would grow past EOF at a full cap: only the
        // in-place tail (len-at = 2) is free, the growth is refused.
        assert_eq!(grant_write(10, 10, 8, 10, 5), 2);
    }
}
