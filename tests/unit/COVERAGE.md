# Unit-test coverage ledger (Phase 2 Layer 1)

Host `cargo test` over the kernel's pure-logic cores, `#[path]`-included from
`../../src/*` so the tests guard the real code. Run: `./run.sh`.

For each core: what the boundary tests cover, and — the *not-claimed* discipline
applied locally — **what they don't**: the impure seam that stayed in the kernel,
and what must hold of the environment for the pure logic's guarantee to hold *in
situ*. Green here proves the arithmetic, not the whole path.

## cpio parser — `src/cpio.rs` (`tests/cpio.rs`)

**Tested (boundaries):** valid lookup + byte-exact data; path normalization
(`/`, `./`); absent file; truncated header (every prefix length — no panic); bad
magic; zero namesize (underflow guard); oversized name past buffer; huge filesize
(overflow guard); trailer stops the scan; `parse_hex`.

**Not covered / seam:**
- The impure seam is `vfs.rs::cpio_data()` — the embedded/relocated archive
  pointer. Tests feed explicit byte slices; they do **not** exercise the
  relocation (`relocate`/`relocate_from` to CPIO_HOME) or the `include_bytes!`
  embedding. If relocation truncates or misplaces the buffer, `lookup` stays
  *safe* (returns `None`) but would fail to find real files — a liveness bug the
  unit tests can't see.
- The other cpio scans in `vfs.rs` (readdir/stat/dir-list, ~lines 313/367/465)
  reuse the pure `parse_hex` but keep their own inline bounds logic — **not yet
  migrated** to `cpio::lookup`, so their bounds-safety is untested here (follow-up).
- **In situ, `lookup`'s guarantee holds iff** `cpio_data()` returns a slice whose
  length is the true archive length (not a stale or over-long pointer).

## RTC decode — `src/rtc_pure.rs` (`tests/rtc.rs`)

**Tested (boundaries):** epoch reference (2021-01-01); leap-day datetime
(2024-02-29 12:34:56); leap rules (1900/2000/2024/2100); Feb-29 differs across
leapness; BCD nibble carry (`0x09`→`0x10`); 12-hour PM/midnight; out-of-range
reject (month/day/hour/year); century-register garbage → 20xx fallback.

**Not covered / seam:**
- The impure seam is `rtc.rs::read_rtc_unix_secs` — the CMOS port I/O, the
  update-in-progress (UIP) wait, and the two-consecutive-reads consistency loop
  that guards against a rollover landing between per-register reads. Tests take
  an already-consistent `RawFields`; they do **not** exercise the UIP loop or a
  torn read. If that loop is wrong, `decode` gets inconsistent bytes and produces
  a plausible-but-wrong time the range check won't catch (e.g. an `HH:59:59`→
  `HH+1:00:00` straddle).
- `days_since_epoch` does not itself reject Feb-29 in a non-leap year (the
  `day <= 31` range check accepts it); tested only that leap/non-leap *differ*,
  not that non-leap Feb-29 is rejected (it is not).
- **In situ, `decode`'s guarantee holds iff** the RTC presents UTC (documented
  assumption) and the century register is either sane (19–21) or absent (0).

## ENA ring index/phase — `src/net/ena/ring.rs` (`tests/ring.rs`)

**Tested (boundaries):** slot wrap at capacity; SQ phase inverts exactly at the
depth boundary (and *not* mid-ring); SQ phase period is two full laps; SQ index
wraps u16 cleanly (depth divides 65536); CQ advance wrap + phase flip;
`entry_ready` is phase equality (bit-0 only); `free_slots` empty/full + the kept
slot; `free_slots` correct across the u16 wrap (avail/used indices disagree).

**Not covered / seam:**
- The impure seam is `device.rs`'s volatile descriptor reads/writes, the doorbell
  MMIO, and DMA coherence. The pure functions decide *which* slot and *when* the
  phase flips; they do **not** verify the descriptor bytes actually reached DMA
  memory, that a doorbell write reached the NIC, or that the device's phase
  convention matches ours. A coherence or doorbell bug yields the same
  intermittent stall the phase math would — invisible here; only a Nitro
  serve-verify catches it.
- Tests assume a power-of-two depth (`IO_DEPTH = 256`) so the `& mask` slot
  arithmetic holds; a non-power-of-two depth would break `slot`/`sq_advance` and
  is not guarded.
- **In situ, the guarantees hold iff** the device honors the ENA phase-bit
  protocol (toggle per wrap) and the completion queue's write-back DMA is coherent.

## VFS fd table — `src/fd_table.rs` (`tests/fd_table.rs`)

**Tested (boundaries):** alloc → find → free → re-alloc reuses the freed slot
(first-None, not leaked); use-after-free lookup is a clean miss, not a stale hit;
double-free is a defined `false` no-op (no panic/corruption); exhaustion returns
`None` (→ caller's -EMFILE), never an OOB write; out-of-range/negative/`i32::MAX`
and unknown fds miss cleanly; stdio fds 0/1/2 are never in the VFS table (a lookup
of them is a clean miss, never aliases a real open file).

**Not covered / seam:**
- The impure seam is `vfs.rs`'s `OPEN_FILES: spin::Mutex<[Option<OpenFile>; MAX_OPEN]>`
  and the `NEXT_VFS_FD` monotonic counter. Tests drive a plain `[Option<OpenFile>; N]`
  slice; they do **not** exercise the lock (so a lost-wakeup or double-lock is
  invisible here) nor the counter's monotonicity (fd *values* come from the atomic,
  not from `alloc`, which only chooses the *slot*). The fd-reuse-safety guarantee —
  that a recycled slot can't be reached by a stale fd number — holds in situ **iff**
  `NEXT_VFS_FD` never wraps to reissue a live number (it's a u64 from 1000; wrap is
  not reachable in any real uptime, so it's assumed, not enforced).
- `alloc` picks the first-None slot; the *fd number* stored in the slot is the
  caller's business. Tests use distinct fd values, so a caller bug that reused an fd
  number across two live slots would produce a `find` ambiguity the pure layer
  can't see (it returns the *first* match) — that's a vfs.rs invariant, not a
  fd_table one.
- **In situ, the guarantees hold iff** every open path routes through `alloc` (no
  slot written behind its back) and every close routes through `free` (no slot
  cleared without going through the table) — both true in vfs.rs today, but that's
  a call-site discipline the unit tests assume, not prove.

## tmpfs path + cap arithmetic — `src/tmpfs_tree.rs` (`tests/tmpfs_tree.rs`)

**Tested (boundaries):** `norm` — leading-slash insertion, `./` strip, trailing-slash
drop, root/`//`/empty → `/`, idempotence; `parent` — nested, one-level, mount-root →
`/`, root → `/`; `is_mount_path` — `/tmp` & `/dev/shm` and their subtrees yes, the
`/tmpx`/`/dev/shmx` prefix-trap no, `/` and outside-mounts no; `is_under` (the
delete-non-empty gate) — direct + deep descendant yes, the `/tmpx`-vs-`/tmp`
prefix-trap no, a dir is not its own child; **`grant_write` (the 4 MiB cap
off-by-one)** — fits below cap, exact-fill *to* the cap boundary (grants the last
byte), at-cap → 0 (ENOSPC, the cap+1 case), a sparse gap past EOF spends budget
(gap+payload ≤ cap), a gap that alone overflows → 0, and **overwrite of existing
bytes is free even at total == cap** (growth is `max(0, at+n-len)`, not `n`).

**Not covered / seam (named gaps, not faked):**
- The impure seam is `tmpfs.rs`'s `spin::Mutex<Option<Tmpfs>>`, the raw user-buffer
  `copy_nonoverlapping` in `write_at`/`read_at`, and the `NEXT_FD`/open-`Vec` fd
  table. The pure layer decides *how many bytes may be written* and *which paths
  normalize/route/nest*; it does **not** touch the actual `Vec<u8>` growth, the
  zero-fill of a sparse gap, or the byte copy — a bug in the resize/copy would be
  invisible here (only boot + Layer-2 catch it).
- **The node-tree create/lookup/delete/collision decisions stay inline in `tmpfs.rs`
  and are NOT host-unit-extracted.** They are a thin branch table over
  `BTreeMap::contains_key/insert/remove` interleaved with the lock, the byte
  accounting, and the open-fd table; extracting them faithfully (so the tests guard
  the *real* store, not a copy) would require delegating the node store itself into
  the core — a subsystem rewrite, not a behavior-preserving Layer-1 extraction.
  Rather than test a divergent copy (the exact fake-coverage failure this exercise
  exists to catch — cf. the serial-ring N/A), they are named here as a gap: covered
  by boot-verify (happy path) and slated for **Layer 2** adversarial (EEXIST,
  ENOTEMPTY, delete-non-empty, deleted-lookup under hostile/`concurrent` input).
- `resize_path` (ftruncate/truncate growth) uses a **distinct** all-or-nothing cap
  check (`total + grow > CAP` → ENOSPC, no partial), *not* `grant_write`'s
  partial-fill logic — deliberately (truncate has no "write as much as fits"
  semantics). It is not routed through the core and not unit-tested; its cap edge is
  a separate, simpler compare.
- **In situ, `grant_write`'s guarantee holds iff** `fs.total` is the true sum of all
  file `data` lengths (every growth/shrink path updates `total` in lockstep — an
  invariant tmpfs.rs maintains at each mutation site, assumed here not proven) and
  the same lock covers the check-and-write (it does; the whole fn holds the mutex).

## serial ring — `src/serial.rs` — N/A (no ring buffer)

**Confirmed N/A, no test written.** `src/serial.rs` is direct 16550 UART port
I/O: `_print` / `raw_str` / `raw_hex` (+ `_nolock` variants) spin on the LSR
transmit-ready bit (port `0x3FD` & `0x20`) and write bytes straight to COM1 (port
`0x3F8`), gated only by a `QUIET` `AtomicBool`. There is no ring buffer, no
head/tail/phase state, no index arithmetic — **no pure logic to extract, and no
test written.** (Serial *input* for the eval shell reads on demand in
`syscall.rs`; also not a ring.) Recorded as a truthful gap rather than a
manufactured test — a green count padded with a fake serial-ring test is exactly
the failure mode this whole exercise exists to catch.
