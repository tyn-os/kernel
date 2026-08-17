# Phase 2 — testing status (what's built, what's deferred-with-reason)

The layered test method, closed. The point was never "build every layer" — it was
high-value coverage with real teeth, gaps **named** not hidden. It found real bugs
(a latent cpio OOB; BUG-8, a remote connection-flood kernel-panic DoS), which is
the measure that matters. Newest layers first.

## Tiers

- **Fast / every-build** (`cargo test`, no qemu, no cloud): Layer-1 unit cores +
  Layer-3 host fuzz. Run: `tests/run-all.sh` (or `cd tests/unit && ./run.sh`).
- **Scheduled** (a running instance; Nitro for anything networking/SMP): Layer-2
  resiliency, Layer-4 soak. Not every-build — they need an instance and take
  minutes–hours.

## Layer status

| Layer | What | Status |
|---|---|---|
| 0 — regression guards | SIMD/red-zone probes, teeth-tested | ✅ committed |
| 1 — unit cores | cpio · rtc · ena-ring · fd-table · tmpfs path/cap, boundary-tested | ✅ committed (45 tests; found+fixed the cpio OOB) |
| 2 — resiliency | slow-loris · fd/conn-flood; tmpfs-cap-under-concurrency | ✅ high-value find banked → **BUG-8 CLOSED** (panic + recovery). Harness: `tests/resiliency/` |
| 3 — fuzz (host) | PRNG hammer of the pure cores, content invariants | ✅ committed (`tests/unit/tests/fuzz.rs`, ~6.5M inputs, teeth-proven) |
| 3 — fuzz (in-situ) | syscall-boundary garbage | ⚠️ **partial — see gaps** |
| 4a — BUG-1 SMP guard | long `-smp 2` md5 soak, `large_md5==0` | 🛠 **harness-ready, run deferred** (`tests/soak/nitro_soak.sh`); teeth already established |
| 4b — drift soak | heap/fd/socket/latency bounded over hours | 🛠 harness-ready (same run); needs VERBOSE for `[diag]` (below) |
| 5 — standing harness | tier runner | ✅ `tests/run-all.sh` (fast tier); scheduled tier documented here |

## Deferred / gaps — NAMED, with reason

- **In-situ fuzzing of memory pointer/size syscall args** (read/write/sendfile) —
  **BLOCKED by the identity-map hazard.** Tyn identity-maps 0–4 GiB, so a bad
  pointer/oversized length **silently corrupts adjacent memory instead of
  faulting** (the SYSTEMIC HAZARD in `BUGS.md`); a fuzzer there would corrupt, not
  cleanly report → inconclusive. This is itself a finding: the identity map limits
  the testability of the most-exposed syscalls → **raises guard-pages priority**
  (a guard page under stacks/buffers turns the class into clean faults and unblocks
  it). The **fd/flag/int** arg space *is* safely in-situ-fuzzable (a kernel harness
  driving the dispatch with garbage ints/flags/fds, asserting clean errors) — the
  next in-situ step, not yet built.
- **Layer-4a/4b run deferred** (not the harness — the multi-hour Nitro run).
  `tests/soak/nitro_soak.sh` deploys the amp under real SMP for `TYN_AMP_RUNTIME_MS`
  and asserts `large_md5==0`. **Teeth already proven** in the BUG-1 closure (poison
  kernel `~/work/tyn-kernel-poison`, IPI-IST reverted → `large_md5` 0→4 in ~240 s;
  fixed → 0/1600 s — see `BUGS.md` BUG-1). Run with `KERNEL=…poison` to re-prove the
  teeth, or the fixed kernel for the soak. **4b `[diag]` drift needs VERBOSE on** —
  `[diag]` is gated behind the default-off VERBOSE flag; a runtime VERBOSE toggle
  (boot.config / serial-console) is the `docs/PAYDOWN.md` observability item.
- **Layer-2 breadth** (malformed HTTP / multipart, section A/C of the resiliency
  plan) — **deferred, optional.** Layer 2 already delivered its high-value find
  (BUG-8); the malformed-HTTP breadth is lower-value adversarial coverage, buildable
  later if wanted.
- **cargo-fuzz (coverage-guided)** — upgrade over the every-build random tier; needs
  cargo-fuzz + clang/llvm installed on the build host. The random tier is the
  zero-setup baseline; coverage-guided reaches edges it misses.

## What "closed" means here
High-value coverage built and committed (Layers 0/1/2/3-host), the two real bugs it
found fixed, the standing harnesses in place (`tests/`), and every remaining gap
named with its reason above — not silently missing. The one deferred *run* (the 4a
soak) is harness-ready with teeth established; launch it as the scheduled job when
the cloud spend is wanted.
