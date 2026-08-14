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
