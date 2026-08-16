# Program-level findings — where defects live

A running log of defects that turned up **and why prior testing missed them** —
kept here, not only in commit history, so the *pattern* (where to look next) is
visible in one place. Each entry is a data point on where bugs concentrate.

## 2026-08-15 — BUG-8: connection-flood kernel-panic DoS (security), found by Layer-2 adversarial

**Defect (security-relevant).** A single unauthenticated client holding
~1000–1250 concurrent TCP connections exhausts the static **16 MiB shared kernel
heap** and the kernel **panics** (`alloc.rs:573`, a failed 32 KiB allocation). The
32 KiB is exactly the **TX buffer** every accepted stream / pool listener reserves
(2 KiB RX + 32 KiB TX ≈ 34 KiB per connection; `src/net/socket.rs`
`LISTENER_TX_BUF_SIZE`, `src/memory/heap.rs`). No connection cap, no backpressure,
no clean reject, no recovery — a remote, unauthenticated, single-client
kernel-death DoS, which contradicts the small-TCB/secure thesis.

**Why prior testing missed it.** Every prior test measured *throughput/capability*
under well-behaved load; none fed a *hostile connection flood*. tmpfs was
carefully capped to protect this very heap — but the socket/accept path was not,
so the flood exhausts the heap tmpfs was capped to preserve. It surfaced exactly
where Layer 2 (adversarial) aimed: crash-under-hostile-input on an unreached path.
The reproducer (`tests/resiliency/fd_flood.py` + `nitro_flood_repro.sh`) pins it
deterministically on real SMP: ramp 250/500/750/1000 healthy → 1250 panic.

**Fix status: CLOSED (2026-08-16) — see BUGS.md BUG-8; two parts.** (1)
Heap-headroom backpressure — `sys_accept` refuses to consume an established
connection while free heap < a 4 MiB reserve (`src/memory/heap.rs::free_bytes()`,
`ACCEPT_HEAP_RESERVE`) → no kernel panic under a sustained 4000-connection flood.
SMP-correct by construction (the shared state is the real heap, no manual counter
to drift). (2) A teardown reaper — the panic-fix exposed a second defect: accepted
flood streams get FIN-closed but the abandoned peers never finish the handshake,
so sockets strand in FinWait forever (`gc_closed_handles` only reaped Closed/
TimeWait), leaking ~34 KiB each (measured 219 ≈ 7.5 MiB) → heap pinned at the
reserve → no recovery. The reaper force-`abort()`s sockets stuck in a half-closed
state past `CLOSING_REAP_MS` (15 s, spares legit closes) → heap recovers. Both
**named/proven by direct measurement** (the `[diag]` heap+socket-state trace):
post-flood `heap_free` 4→11.5 MiB, `closing` 219→0, /health recovers, 80/80
legit-close churn served. An earlier accepted-stream **count** cap failed because
the per-connection cost (~34 KiB) makes a safe count sit below the panic point —
cap the resource that actually fails (free heap), not a proxy.

**The data point.** Same lesson as BUG-1 and the cpio OOB: defects concentrate on
paths real workloads never take (here, hostile connection floods). And: a *count*
cap that mis-models per-unit cost is worse than useless (it sits above the failure
point and never engages) — cap the resource that actually fails (free heap), not a
proxy.

## 2026-08-14 — latent cpio OOB / underflow, found by extraction + boundary testing

**Defect.** `vfs.rs::cpio_lookup` computed `name_start + namesize - 1` and
`data_start + filesize` with unchecked arithmetic and unchecked slicing. A
malformed archive with `namesize == 0` underflows `namesize - 1` (panic in a
debug build, wrap in release); oversized name/file size fields overflow the adds
*before* the `> data.len()` bounds check, defeating it. Result on hostile input:
a panic or an out-of-bounds read — the classic remote-input parser hazard, on the
path that loads every `.beam`.

**Why every prior test missed it.** The embedded cpio archive is always
well-formed. No boot, no capability test, no probe ever handed the parser a
malformed header — so the happy path was exercised constantly and the failure
path never once. A green suite that only feeds valid input proves the parser
parses, not that it *refuses* garbage safely.

**How it surfaced.** Phase-2 Layer-1: extracting the pure parser into
`src/cpio.rs` and writing boundary tests (truncated / bad-magic / zero-namesize /
oversized-name / huge-filesize) against it. The zero-namesize and size-overflow
cases are the teeth. Fixed with `checked_add`/`checked_sub` + `slice::get()` + a
`namesize == 0` guard — malformed input now returns `None`, never panics/OOBs.
Boot-verified behavior-preserving on the real (well-formed) archive
(`AMP_BEGIN=1`, `small_md5=0 large_md5=0`, no fault).

**Crash eliminated, not relocated — caller trace.** Returning `None` on malformed
input only helps if callers don't then `.unwrap()` it. Audited: `cpio_lookup` has
exactly two callers, neither of which unwraps — `vfs::exists` maps `None → false`
(`.is_some()`), and `vfs::open` maps `None → return -2` (-ENOENT). No `unwrap` or
`expect` on the lookup exists anywhere in `src/`. So the malformed path traces
end to end: malformed archive → `lookup` returns `None` (unit-tested) → `open`
returns `-ENOENT` / `exists` returns `false` → no panic at the parser *or* the
caller. (The parser's boundary tests cover the parser half; this audit covers the
caller half — the well-formed boot-verify only exercises the happy path.)

Note the caller-side `None` arm is not merely audited — it is exercised at *every*
boot: the kernel does many `cpio_lookup`s for paths that are legitimately absent
(the newc format has always returned `None` on not-found), and each takes the
same `None → -ENOENT / false` arm a malformed archive now takes. The change only
moved malformed input from "panic in the parser" onto that already-boot-proven
`None` path.

**The data point.** Defects concentrate on the paths real workloads never take
(cf. BUG-1: the SMP-only IPI vector, exercised by nothing until a probe forced
it). The Layer-1/2/3 program — unit boundaries, adversarial input, syscall fuzz —
is aimed squarely at those unreached paths, because that is where the next one
will be.
