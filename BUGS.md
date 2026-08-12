# Tyn — tracked bugs

Open defects found during investigation, kept here so they don't seed the next
mystery. Record what was **measured**, not inferred. Newest first.

---

## SYSTEMIC HAZARD — the identity map hides every stack under/overflow

**Bigger than any single bug.** Tyn identity-maps 0–4 GiB, so a stack under/over-run
**does not fault** — it silently writes into adjacent mapped memory (e.g. the BEAM
heap) → downstream corruption (`size_object: bad tag`) instead of a clean `#PF`.
Every stack-bounds bug kernel-wide is therefore converted from a debuggable crash
into silent data corruption. The red-zone naive-fix crash (BUG-1 below) is the first
thing to expose it. **Implication:** guard pages under thread stacks would turn this
whole class into clean faults — reframed from "nice-to-have" to "the thing that
would have made this class debuggable." Feed into the hardening backlog.

---

## BUG-1 — preemption trampoline clobbers the interrupted thread's SysV red zone

**Severity:** high (silent wrong results — `:erlang.md5`/`binary.copy` return wrong
values under preemption; the "md5 large-binary flakiness" from the dist work).
**Status:** **REOPENED — Path A is a strong PARTIAL mitigation, not a complete fix.**
It landed (`src/interrupts.rs` + `src/sched.rs`) and passed UP acceptance (16/16, TCG
`-smp 1`), and it **eliminates the crash mode**. But the first-ever **SMP** test (real
Nitro, c5.large = 2 schedulers; 2026-08-10) measured a **residual md5 corruption**:
`large_md5` climbed 0→4 over 240 s / 52 k iters (monotonic, no crash), read by the
teeth-tested `ampweb` HTTP amplifier (which reads exactly 0 on this same kernel under
UP). **Every prior Path A validation was UP (`-smp 1`), so the SMP path was never
exercised** — and the original "md5 large-binary flakiness" was itself an SMP (Nitro
dist) symptom. Net: red-zone clobber fixed for UP; an **SMP-specific corruption
residual remains**. Do NOT treat BUG-1 as closed.

**Root cause:** `sched_yield_trampoline` (`src/interrupts.rs`) redirects a preempted
thread to `syscall(sched_yield)` by writing the saved RIP + rax/rcx/r11 to
`[user_rsp-8..-32]` — **inside the interrupted thread's 128-byte SysV red zone**
(handler does `new_rsp = user_rsp - 8`, no red-zone skip). BeamAsm/ERTS leaf hot
loops (md5/bincopy) that spill into their red zone get that stack memory clobbered
on preemption → wrong digest/copy. Transient (recovers on recompute) because it's
per-preemption timing, not a persistent memory clobber.

**Measured chain (each link a probe that could have refuted):**
- **dose-response:** amplifier mismatch scales with preemptive context-switch count
  — `PREEMPT_DIV` 1/4/16/64/0 → 42/7/7/1/**0**;
- **fully-off = 0** across 5/5 runs → **one** preemption-sensitive mechanism, no
  preemption-independent residual;
- **4 scalar probes all 0** — FS_BASE, GP(r12–r15), XMM(all xmm0–15), RFLAGS/DF, at
  full preemption + 16-worker dose. **NEGATIVE RESULT — the corruptor is NOT a
  register; it's memory.** (This closed the whole register hypothesis space and
  forced the memory pivot — a first-class finding.)
- **red-zone sentinel clobbered** — `redzone_probe` (`beam-build/nifs/fsbase_probe.c`)
  holds a sentinel in `[rsp-8..-128]` across a preempted spin; it comes back clobbered
  → corruptor named;
- **naive fix** (skip 128 B: `new_rsp = user_rsp-128-8`, `ret 128`) → amplifier
  **21→0** (0 across 8 runs), redzone_probe **clobbered→0** — mechanism AND symptom to
  zero, mechanism measured **before** the fix. **BUT** it introduced a measured
  **25% small-stack-underflow crash** (FIXED 2/8 vs UNFIXED 0/8): reserving 128 B
  below a small dirty/aux thread's rsp underflows into adjacent heap (see systemic
  hazard) → `bad tag`. Fix direction correct, **naive implementation reverted.**

**Fix (Path A — landed):** the trampoline's context moved off the user stack
entirely. The timer handler reads the current thread's kernel-stack top via
`gs:[0]`, builds an `iretq` frame in a **per-thread PREEMPT REGION reserved ABOVE
that top** (`PREEMPT_REGION_SIZE = 256`), and `iretq`s into the trampoline with
**IF=0** (frame's IF cleared) so no nested preemption → one per-thread frame
suffices. The trampoline saves the syscall-clobbered regs (rax/rcx/r11) *below* that
frame, calls `sched_yield`, and ends in `iretq` — restoring orig RIP + user_rsp +
interrupted RFLAGS (IF=1) atomically. The red zone is **never touched**, so both the
original clobber AND the naive-fix's small-stack underflow are structurally
impossible. The region is reserved by every live kernel-stack allocator: `sched.rs`
`KSTACK_NEXT` bumps by `+PREEMPT_REGION_SIZE`; thread 0's `syscall_stack_0` has free
dead-neighbor space above it (see `docs/STACK_ALLOCATOR_INVENTORY.md`). The IF=0
window (`check_resched`→`process_rescues`+`yield_current`→`context_switch`) was
verified bounded & non-blocking before committing to a single-frame region.

**Measured UP acceptance (TCG, `-cpu max -accel tcg`, `-smp 1`, 16-worker dose):**
- amplifier **`large_md5=0` 16/16** and **`crash=0` 16/16** (dual acceptance PASS);
- **`redzone_probe bad=0` 3/3** — the sentinel that named the corruptor comes back
  clean. `redzone_probe` stays the **standing UP guard**.
- **Caveat now understood:** all of this was `-smp 1`. It proves the UP red-zone path
  is fixed; it says **nothing about SMP** (see the SMP residual).

**SMP residual (REOPENS the bug — measured 2026-08-10, real Nitro c5.large):** Path A
kernel (production beam `a9048ee0`, HEAD `922bd5f`, provenance-gated) + the
teeth-tested `ampweb` amplifier under ~4 min sustained load:
- **`large_md5` 0→4** (monotonic: first nonzero at ~t+120 s / 26 k iters, reaching 4
  by 52 k iters), `small_md5=0`, **no crash** (`/health` 200 throughout).
- The instrument is trustworthy here because it was **teeth-tested**: same `ampweb`
  reads **`large_md5=0`** on this exact Path A kernel under UP TCG (110 s, 14 k iters),
  and reads **`large_md5=5` + a node crash** on the unfixed kernel (`0b258c3`, same
  production beam) — so it fires on both BUG-1 failure modes and is silent on the UP
  fix. The Nitro `4` is therefore a **real, timing/SMP-dependent residual UP hid.**
- **SMP confirmed as the defect zone (measured, same kernel + disk, only `-smp`
  differs):** UP `-smp 1` TCG = **clean** (14 k iters, no crash, no corruption);
  `-smp 2` TCG = **crashes in ~30 s at ~89 iters**; Nitro 2-CPU = **corrupts**
  (`large_md5=4`) over 4 min, no crash. UP clean + both SMP configs broken ⇒ a real
  Path A SMP defect.
- **`-smp 2 -accel tcg` is a DEAD END (MTTCG artifact) — measured, don't reuse it.**
  It crashes *any* app in ~20–30 s under load: Path A + ampweb died at ~89 iters, and
  a *simple* app (clock2, no amplifier) crashed by ~t+20 s under mere HTTP load. But
  **real Nitro SMP ran the far heavier ampweb for 4 min without crashing** (it
  corrupted, stayed alive). Light-app-crashes-emulated vs heavy-app-survives-real ⇒
  the `-smp 2` TCG crash is TCG's own multi-core emulation being unfaithful, **not** a
  Tyn SMP bug and **nothing to do with Path A or the corruption**. Filed as its own
  note; do not use `-smp 2 -accel tcg` for SMP validation.
- **FAITHFUL LOCAL REPRODUCER FOUND (2026-08-12).** UP `-smp 1` TCG is clean (can't see
  it); `-smp 2` TCG is an MTTCG artifact (crashes before it can measure); the build host
  (m7i.large, a Nitro VM) can't nest KVM. **A c5.metal running `qemu -accel kvm -smp 2`
  reproduces the corruption** — measured, controlled A/B on one box, only `-smp` differs:
  - **`-smp 1` control: `large_md5 = 0`** (38 k iters, no crash);
  - **`-smp 2`: `large_md5 = 5`** (monotonic 0→1→2→4→5, first nonzero ~19 k iters,
    71 k iters, no crash) — **matches Nitro's signature** (Nitro: first nonzero ~26 k,
    reached 4 by 52 k). Nitro-matched dose (16 workers + 30-way HTTP hammer, 240 s); **no
    escalation needed.**
  - **Recipe (for the hunt):** c5.metal → `qemu-system-x86_64 -accel kvm -cpu host -m
    2560M -smp 2` booting `fixed.raw` (Path A + ampweb, `-m 2560M`) + a 30-way `/health`
    hammer; read `large_md5` on `/chk`. Boot ~6 s, so **seconds-per-iteration** vs
    ~10 min/Nitro-AMI. `-smp 1` on the same box is the built-in clean control. Harness:
    `tests/simd/` + `~/work/run_one.sh` (`ACCEL CPU SMP PORT DISK HAMMERP SECS`).
  - Three earlier metal runs failed on *my harness bugs* (no hammer / `-m 3072M` boot
    `#PF` / `$(hammer…)` pipe hang), NOT real nulls — see
    `feedback_expensive_cloud_harness`. The clean run followed Rule 0 (validate the exact
    script on TCG first, `(…) & HP=$!`, deadman-switch, confirm teardown).
- **Suspects (measure, don't assume):** the omitted trampoline FXSAVE/`gs:[48]` SIMD
  scaffolding (task #74, dropped as UP-dead) needed under SMP; per-CPU vs per-thread
  preempt-region assumptions; `context_switch` under 2 schedulers; AP APIC-timer
  preemption on the region path.
- Reproducer/instrument: `tests/simd/ampweb/` (HTTP `/chk` `large_md5`, `/health`) —
  the proven detector; deploy Path A + it to Nitro to read the residual.

**Three earlier wrong turns (each caught by a refutable probe — recorded so they
don't recur):**
1. **XMM jump** — hypothesised XMM-clobber; refuted (xmm_probe, 0/95k, and md5's core
   is scalar). The narrow old probe (xmm0/xmm1) was a *near*-false-negative; the
   hardened all-xmm0–15 probe later also read 0.
2. **`context_switch` duplicate misread** — indicted `thread.rs:386` (GPRs-only) for
   omitting FS_BASE; the yield path actually uses `sched::context_switch`
   (`sched.rs:1184`), which DOES save/restore FS_BASE + fxsave. thread.rs:386 is a
   separate clone/init-path switch.
3. **FS_BASE promoted premise** — a directions file treated "FS_BASE fixed, amplifier
   30→0" as measured when it wasn't; the OFF/ON toggle proved FS_BASE **irrelevant to
   the amplifier** (fs_base preserved AND amplifier still corrupts).

Reproducers/probes: `tests/simd/` (amplifier + probe runners) +
`beam-build/nifs/{fsbase_probe,xmm_probe,canary}.c`.

## Unification (BUG-1 / BUG-4 / #72) — TESTED → **kept separate** (no merge)

BUG-1, BUG-4 (boot `#PF`), and GP_HUNT #72 (tmpfs `#GP`) share a **plausible** cause
(red-zone clobber is a general memory corruptor → a beam pointer spilled to the red
zone and clobbered would fault wild). The Path-A unification test ran a pointer-heavy
(tmpfs) workload on both trees: **wild-pointer-fault boots = 0/8 unfixed AND 0/8
Path A.** The wild-pointer fault did **not reproduce on either tree**, so the fix
neither confirmably kills it nor is refuted — **absence proves nothing.** Verdict:
**keep separate, no merge** (merging on an unreproduced coincidence is exactly the
`0x100000000` trap that already once correctly *split* BUG-1 from BUG-4). If BUG-4/#72
resurface with a live reproducer, re-run the test then.

## BUG-4 — boot `#PF` at `cr2=0x100000000` (beam.smp reads a wild ~4 GiB pointer)

**Severity:** high (crashes boot under QEMU/TCG). **Status:** symbolized; part of the
unproven unification above.

`demo-live.log:1668 #PF ip=0x989c81 cr2=0x100000000`. `0x989c81` is **beam.smp `.text`**
(not kernel — the original "kernel ip" read was the wrong binary), a heavily-unrolled
**byte-block hash** faulting on `movzbl 0x8(%rcx)` with `rcx≈0xFFFFFFF8` — a wild ~4 GiB
pointer walking off the top of the 4 GiB identity map. cr2 clusters at `4GiB+{0..0xb}`
across the corpus (not a fixed fingerprint — just the map wall). Not reproduced in the
current shipped tree (0/16 boots incl. amplifier); the reliable historical faults were
on experimental (FXSAVE/futex-valve) builds. Left open pending the BUG-1 unification
test.

## BUG-5 — current `main` no longer serves on Nitro (regressed since Aug-8)

**Severity:** high (blocked all real-hardware validation, incl. BUG-1's Nitro residual).
**Status:** **RESOLVED** — the regression was the uncommitted canary-beam swap, not a
source commit. Confirmed on Nitro (2026-08-10): the clean-clone kernel with the
**production beam `a9048ee0`** + `clock2.cpio` **served `/health` in ~8s** on c5.large
(`HEAD=39e9959`, tree clean, logged by the new provenance gate). No bisect needed —
CLEAR_DECK Step 1 (real git clone → restores the production beam) resolved it. The
`deploy-ami.sh` provenance gate now prevents an untracked beam from shipping again.

**Root cause (was the prime suspect, now confirmed): the embedded beam, not a source
commit.** Making the build host a real git clone (CLEAR_DECK Step 1) exposed that
`src/beam.smp.elf` on the host was **`c5461aee` = `beam_canary.smp`, a red-zone-hunt
probe beam left embedded**, while git tracks the production beam **`a9048ee0`
(`beam-crypto.smp`, backed up on the host as `beam.smp.elf.orig`)**. The swap-in date
(`beam.smp.elf.orig` created **Aug-9 01:14**) draws a clean boundary: **Aug-8 deploy
served → production beam; every Aug-9-01:14-onward deploy no-serve → canary beam.** So
the regression is very likely the *uncommitted probe-beam swap*, and the clean clone
(which restores `a9048ee0`) may fix it outright — no commit-bisect needed. **Test the
clean-clone kernel (production beam) on c5.large first;** only bisect if it still fails. The Aug-8 known-good deploy (stock kernel + `clock2.cpio`) served at
`18.206.85.142:8080`. **Now, the same pipeline (`deploy-ami.sh` → `build-disk.sh` →
import-snapshot → `register-image --boot-mode legacy-bios` → `run-instances`, c5.large)
produces NO-SERVE for the stock kernel** (`tyn-kernel-unfix` = current-main-minus-Path-A)
with the *same* `clock2.cpio`. Full matrix, all NO-SERVE now: {Path A, stock} ×
{demo-rootfs, clock2}. Because **stock also fails**, this is **not** Path A — it's a
regression in committed `main` (or the pipeline/EC2 env) somewhere in the Aug-8→now
window (RTC clock, tmpfs, dist, sendfile, and pipeline edits all landed there).

**Constraint that makes this hard:** Tyn's serial console does **not** reach EC2
`get-console-output`, so a NO-SERVE gives no boot log — the only Nitro observability is
HTTP on :8080. Next-session plan: **bisect** the Aug-8→now commits (deploy the Aug-8
kernel binary as a positive control first to prove the pipeline/env still works, then
walk forward), or diff the security-group / `build-disk.sh` / `deploy-ami.sh` against
their Aug-8 state. **Prerequisite (RESOLVED):** the build host `~/kernel` is now a real
clone of origin/main (CLEAR_DECK Step 1), so `git bisect` → build → test is possible —
but likely unnecessary given the beam suspect above. `deploy-ami.sh` now gates on a clean
tree + logs the built HEAD SHA, so any bisect deploy is traceable to its commit. Do this on **c5.large** (cheap) with the leak-proof terminate-on-exit
trap + `Instance:`-anchored id extraction (a prior regex bug leaked a c5.metal once).

## BUG-6 — boot `#PF` under qemu-kvm at `-m 3072M` (`-m 2560M` boots fine)

**Severity:** medium (a memory-size-dependent boot crash; latent — could bite on any
host/config that hands Tyn a larger RAM map). **Status:** open, reproduced, not
diagnosed. Found while building the SMP reproducer: booting `fixed.raw` (Path A + ampweb)
under `qemu -accel kvm -smp {1,2} -m 3072M` **faults at boot** —
`#PF ip=0xf030ae4 cr2=0x380000000014 rsp=0x11ae35e8` on **both** `-smp 1` and `-smp 2`
(so it is **not** SMP-related). The **same disk boots cleanly at `-m 2560M`** (used
throughout for both the reproducer and the Nitro deploys), so it is purely a function of
the guest RAM size / e820 map. `cr2=0x380000000014` (~3.5 TiB) is a wild pointer — likely
Tyn's memory-map/e820 handling computing an out-of-range address when the top of RAM sits
at 3 GiB rather than 2.5 GiB (cf. the 4 GiB identity-map ceiling, BUG-4 class). **Repro:**
`qemu-system-x86_64 -accel kvm -cpu host -m 3072M -machine q35 -smp 1 -drive
file=fixed.raw,…` on a KVM host → boot `#PF` on serial. **Do not "improve" the reproducer
harness's `-m` — 2560M is the known-good value.** *Fix direction:* audit Tyn's e820/RAM
sizing (`src/main.rs` boot path) for an assumption that breaks above ~2.5 GiB.

## BUG-2 — `tyn_boot` crashes `exit_group(127)` on a config env value of `"0"`

**Severity:** medium. **Status:** open. A boot.config env var whose value is the string
`"0"` (e.g. `tyn-pack --env TYN_AMP_CHURN_KB=0`) makes the node `exit_group(127)` at
boot; non-"0" values boot fine. **Dangerous face:** it corrupted a *measurement* — it
blocked the churn=0 baseline, producing a false "churn-driven" reading of BUG-1 for two
reports before a `churn_type=none` run refuted it. A config bug that silently poisons a
test's control is worse than one that just crashes boot.

## BUG-3 — beam.smp build can't link two static NIFs at once

**Severity:** low (tooling). **Status:** open. `build-beam.sh --nif-modules "a b"`
produces `--enable-static-nifs=/build/nifs/a.a,/build/nifs/b.a` and the ERTS build
mangles the comma-list into one path (`ld: cannot find …a.a/build/nifs/b.a`). Single
NIF builds fine. Worked around by putting all probes in one module (`fsbase_probe.c`).

## Latent — `arch_prctl(ARCH_SET_FS)` doesn't update `ctx.fs_base`

**Severity:** low (currently moot). **Status:** filed. `sys_arch_prctl(ARCH_SET_FS)`
(`syscall.rs`) writes the FS_BASE MSR but never updates the saved `ctx.fs_base`. Moot
today because `sched::context_switch` (sched.rs:1184) reads the *live* fs_base via
`rdmsr`, not the saved copy — would bite if any path ever trusted `ctx.fs_base`. **Not
BUG-1.**
