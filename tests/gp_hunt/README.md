# GP_HUNT — the "tmpfs large-write #GP"

## The finding (it is not a tmpfs bug)

The `#GP` labelled "large tmpfs write" is the **SMP red-zone memory-corruption class**,
surfaced by concurrent file I/O — **not** a tmpfs defect. Evidence:

1. **Prior pin (`directions/GP_PROBE_A0.md`):** the fault RIPs sit in **BeamAsm JIT
   code** (above the static `beam.smp` image, below the kernel), dereferencing a
   **corrupted binary pointer** into non-canonical space → `#GP`. tmpfs was only the
   *workload* — it put enough large parallel `sys_write` copies in flight (ERTS `+A`
   async pool) to widen a race window. Rare, history-dependent → race-shaped, SMP-only.
2. **Static read of `src/tmpfs.rs` (this session):** the write core is memory-safe —
   `grant_write` returns `count.min(max_n)` (no user-buffer overread), the copy is
   bounded, `new_end` is capped near the 4 MiB cap. The fault is not in tmpfs.
3. **Same class as the `erlang:md5` large-binary flakiness** — which was root-caused as
   **BUG-1**: the SMP wakeup IPI (vector 34) missing its IST, so it pushed its frame
   into the user SysV red zone and clobbered spilled registers under preemption. A
   clobbered spilled *value* → wrong md5; a clobbered spilled *binary pointer* → wild
   deref → `#GP`. Same mechanism, different victim register.

**BUG-1 is fixed in the current tree** (`7270266`; `src/interrupts.rs` vector 34 now
`.set_stack_index(0)`, Nitro-validated, regression-suited). The BUGS.md unification test
already couldn't reproduce the `#GP` on either tree. **So the prime hypothesis: BUG-1's
fix closed this too.** Unproven for the file-I/O *surface* → this harness verifies it,
reproduce-first, with teeth.

## The harness

- **`gp_repro.exs`** (`GpHunt`) — concurrent large tmpfs writes under the `+A` pool,
  byte-exact `===` verification (NEVER `erlang:md5` — itself flaky on large binaries
  here). Two-sided detection: hard `#GP` (crashes; loud kernel handler prints `#GP ip=`)
  **or** silent corruption (readback mismatch). Live bytes kept under the 4 MiB cap so
  the large copies run instead of starving on ENOSPC.
- **`gp_app/`** — boot-runs `GpHunt` and prints `GP_REPRO_RESULT` to serial (+ `/gp`,
  `/health`). Params via env: `GP_PROCS`, `GP_SIZE`, `GP_ITERS`. Scaffold from `l2app`
  (bandit), same pattern as `tests/soak/tls_cluster`.
- **`nitro_gp.sh`** — STAGED dual-acceptance run on real Nitro SMP (leak-proof + deadman).

## Free validation done (this session)

- Reproducer + app compile; plumbing **PASS** under TCG `-smp 1`: 120 write/read/verify
  cycles byte-exact, no `#GP`, no corruption, result on serial + `/gp`.
- **TCG cannot exercise the SMP race:** `-smp 2` under TCG hits an *unrelated* boot `#PF`
  (`ip` in kernel, `rsp=0x80`, wild `cr2`) during AP bringup — a TCG-SMP-emulation /
  KVM-not-TCG-class boot fault, distinct from the JIT `#GP` (which is a `#GP` under load,
  not a kernel `#PF` at boot). Noted, not chased. So the authoritative run is **Nitro**.

## Nitro run result (2026-08-20) — INCONCLUSIVE for the tmpfs surface

Dual-tree (poison vs fixed-29abbab), 4-vCPU SMP, 4500 cycles/tree: the file-I/O repro
triggered **no real `#GP` on either tree** — both `GP_REPRO: PASS`, mismatches=0, node
survived. The initial "`#GP=1` on both" was a **harness FALSE POSITIVE**: `grep '#GP ip='`
matched the reproducer's own PASS advisory (which quoted `'#GP ip='`), not a fault. Verified
by a full-console capture (`grep '#GP ip=0x… rsp='` = none); fixed at source (PASS message no
longer contains the literal; the greps anchor on `#GP ip=0x<hex> rsp=`). So the file-I/O
**surface is too weak to reproduce the rare corruption `#GP` even on poison** — the
**class-level closure by BUG-1 stands on the md5 amplifier** (deterministic), not on this run.
Lesson: the artifact you grep must be the artifact you claim (over-read twice before the
capture caught it). $0 leaked. `gpf_handler` HALTS the faulting core (fatal-per-core).

## The teeth (design, if re-run with a stronger trigger)

`SG_ID=... tests/gp_hunt/nitro_gp.sh` runs `gp_app` on BOTH kernels (c5.xlarge = 4 vCPU
real SMP):

- **POISON** (`~/work/tyn-kernel-poison`, IPI-IST reverted): expect the repro to surface
  the class (mismatch or `#GP`). Historically the file-I/O surface is a **rare** trigger,
  so if poison stays clean, that's a *weak-trigger* result — confirm the class is live on
  poison via the deterministic `ampapp` md5 amplifier (`tests/soak/nitro_soak.sh` with
  `KERNEL=~/work/tyn-kernel-poison`), which faults `large_md5>0` on poison.
- **FIXED** (current kernel): expect **CLEAN** — no `#GP`, `mismatches=0` — over enough
  iterations that silence is signal (`GP_ITERS` high).
- **Verdict:** fixed-clean AND (poison-surfaces OR ampapp-confirms-class-live) ⇒ the
  `#GP` is closed by BUG-1's fix.

## Acceptance / after

- Update the README known-issue line per the outcome: resolved → drop the "Large tmpfs
  writes can `#GP`" bullet; if only partially, state accurately.
- Guard-pages remain the roadmap hardening (turns "walk off the end → `#GP`/silent
  corruption" into "clean fault at a guard page").
- **No commits unless asked.**
