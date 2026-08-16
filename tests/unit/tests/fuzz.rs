//! Phase-2 Layer-3 (fuzz) — host-side, coverage-*un*guided but high-volume random
//! hammering of the attacker-influenced pure cores, with CONTENT invariants (not
//! just "didn't panic"): the class this catches is "hostile/garbage input crashes
//! or corrupts" — the cpio OOB (Layer 1) was exactly this, one layer up. A panic
//! or a returned out-of-bounds range fails the test.
//!
//! Deterministic xorshift PRNG (fixed seed) so a failure reproduces. No deps.
//! Upgrade path: cargo-fuzz/libfuzzer for coverage-guided edge finding (needs the
//! toolchain installed) — this is the every-build, zero-setup tier.
#[path = "../../../src/cpio.rs"]
mod cpio;
#[path = "../../../src/tmpfs_tree.rs"]
mod tmpfs_tree;
#[path = "../../../src/fd_table.rs"]
mod fd_table;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn range(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() as usize) % n }
    }
    fn bytes(&mut self, out: &mut Vec<u8>, max: usize) {
        out.clear();
        let len = self.range(max + 1);
        for _ in 0..len {
            out.push((self.next() & 0xff) as u8);
        }
    }
}

const ITERS: usize = 2_000_000;

/// cpio::lookup / parse_hex on random archives + random paths. INVARIANT: a
/// returned (offset, len) must lie fully within the input buffer — a parser that
/// returns an out-of-bounds range is an OOB waiting to happen (the Layer-1 bug
/// class), caught here even if it doesn't panic on this input.
#[test]
fn fuzz_cpio_lookup_never_returns_oob() {
    let mut rng = Rng(0x1234_5678_9abc_def0);
    let mut data = Vec::new();
    let mut path = Vec::new();
    for _ in 0..ITERS {
        rng.bytes(&mut data, 300);
        rng.bytes(&mut path, 40);
        if let Some((off, len)) = cpio::lookup(&data, &path) {
            // The only allowed successful return is an in-bounds slice.
            let end = off.checked_add(len).expect("cpio::lookup off+len overflowed");
            assert!(
                end <= data.len(),
                "cpio::lookup returned OOB range off={off} len={len} for data.len()={} path={:?}",
                data.len(),
                path
            );
        }
        // parse_hex must never panic on arbitrary 8-byte windows.
        if data.len() >= 8 {
            let _ = cpio::parse_hex(&data[..8]);
        }
    }
}

/// tmpfs_tree path helpers on random bytes: must never panic, and norm must be
/// idempotent (norm(norm(x)) == norm(x)) — a normalization that isn't idempotent
/// is a routing hazard (owns_path / mount checks depend on it).
#[test]
fn fuzz_tmpfs_paths_never_panic_and_norm_idempotent() {
    let mut rng = Rng(0xcafe_babe_dead_beef);
    let mut p = Vec::new();
    let mut d = Vec::new();
    for _ in 0..ITERS {
        rng.bytes(&mut p, 48);
        rng.bytes(&mut d, 48);
        let n = tmpfs_tree::norm(&p);
        assert_eq!(tmpfs_tree::norm(&n), n, "norm not idempotent on {:?}", p);
        let _ = tmpfs_tree::parent(&n);
        let _ = tmpfs_tree::is_mount_path(&n);
        let _ = tmpfs_tree::is_under(&p, &d);
    }
}

/// grant_write on random (total, cap, at, len, count). INVARIANT: the granted
/// byte count never lets accounted bytes exceed the cap — i.e. when total <= cap,
/// total + growth(granted) <= cap. This is the tmpfs cap the flood/upload paths
/// rely on; a violation is a heap-overrun/ENOSPC-escape. Values span a wide range
/// to probe the arithmetic (incl. overflow) but bounded so the ASSERT itself is
/// exact.
#[test]
fn fuzz_grant_write_respects_cap() {
    let mut rng = Rng(0x0f0f_0f0f_1234_9999);
    // 30-bit range: adversarial (up to ~1 GiB) but no usize overflow in checks.
    let m = |r: &mut Rng| (r.next() as usize) & ((1 << 30) - 1);
    for _ in 0..ITERS {
        let cap = m(&mut rng);
        let total = m(&mut rng);
        let at = m(&mut rng);
        let len = m(&mut rng);
        let count = m(&mut rng);
        let n = tmpfs_tree::grant_write(total, cap, at, len, count);
        assert!(n <= count, "granted more than requested: n={n} count={count}");
        if total <= cap {
            // growth = bytes the file grows to place the granted n bytes at `at`.
            // Only an actual write (n>0) grows anything; n==0 writes nothing, so
            // growth is 0 (a 0-byte write does NOT extend the file to `at`).
            let growth = if n == 0 { 0 } else { (at + n).saturating_sub(len) };
            assert!(
                total + growth <= cap,
                "grant_write let total exceed cap: total={total} cap={cap} at={at} len={len} count={count} n={n} growth={growth}"
            );
        }
    }
}

/// fd_table alloc/find/free over a small table under random ops: the live set is
/// always consistent — every alloc'd fd is findable until freed, freed fds miss,
/// and alloc never indexes out of bounds (returns None when full).
#[test]
fn fuzz_fd_table_stays_consistent() {
    use fd_table::{alloc, find, free, OpenFile};
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let of = |fd: i32| OpenFile { fd, data_offset: 0, data_len: 0, pos: 0 };
    for _ in 0..(ITERS / 4) {
        let mut t: [Option<OpenFile>; 8] = Default::default();
        let mut live: Vec<i32> = Vec::new();
        for _ in 0..40 {
            let fd = (rng.range(20) as i32) - 2; // includes negatives / stdio range
            match rng.range(3) {
                0 => {
                    // alloc if fd not already live
                    if !live.contains(&fd) {
                        if let Some(i) = alloc(&mut t, of(fd)) {
                            assert!(i < 8);
                            live.push(fd);
                        } else {
                            assert_eq!(live.len(), 8, "alloc returned None but table not full");
                        }
                    }
                }
                1 => {
                    let hit = free(&mut t, fd);
                    if hit { live.retain(|&x| x != fd); }
                }
                _ => {
                    let found = find(&t, fd).is_some();
                    assert_eq!(found, live.contains(&fd), "find disagrees with live set for fd={fd}");
                }
            }
        }
    }
}
