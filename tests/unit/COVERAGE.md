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
