# Paydown — accreted debt, named

Deferred cleanups and infra debt found across the arc. Kept in one named list so
it doesn't evaporate — several items are the **"small thing left undone that bites
later"** class this project keeps re-learning (the `.cargo/config.toml`
showstopper, the `tests/*` gitignore, the config-value-`"0"` crash). Open *defects*
live in `BUGS.md`; this is the debt-to-pay-down view, cross-referenced where they
overlap. Not ranked; each notes what it is, why it bites, and the fix if known.

## Config / deploy drift (the highest-bite class)

- **✅ RESOLVED (Aug-10) — the build host `~/kernel` is now a real git clone.**
  It was a mutable hand-synced tree with no `.git`, which **silently broke the
  artifact-matches-git guarantee all validation rests on** — third recurrence of the
  class (kin: `.cargo/config.toml` showstopper, `tests/*` gitignore). CLEAR_DECK
  Step 1 replaced it with a clean clone of origin/main (old tree kept as
  `~/kernel-prev`), and `deploy-ami.sh` now **gates on a clean tree + logs the built
  HEAD SHA + beam hash** (override: `PROVENANCE_ALLOW_DIRTY=1`). **What the gap had
  been hiding:** the host's embedded `src/beam.smp.elf` was `c5461aee` = a red-zone
  probe beam (`beam_canary.smp`) left in place, not git's production `a9048ee0` — i.e.
  every recent deploy shipped an untracked probe beam. That is now the **prime suspect
  for BUG-5** (Nitro no-serve). *Residual discipline (still owed):* host-only build
  inputs (`clock2.cpio`, the probe beams in `beam-build/`, and `beam.smp.elf` itself
  when rebuilt) should be reproducible-from-repo or hash-and-recorded — ideally make
  `beam.smp` buildable from `beam-build/` and commit rebuilt beams, so "which beam did
  this deploy with" can never drift untracked again.
- **Nitro serve regressed since Aug-8 (BUG-5).** The stock kernel + `clock2.cpio`
  served on Nitro Aug-8 but the same pipeline NO-SERVEs now — measured, stock kernel,
  so **not** Path A. Blocks *all* real-hardware validation (incl. BUG-1's Nitro
  residual). *Fix:* bisect the Aug-8→now commits (deploy the Aug-8 kernel as a
  positive control first), or diff SG/`build-disk.sh`/`deploy-ami.sh` vs Aug-8.
  Hampered by Tyn serial not reaching EC2 console (HTTP :8080 is the only signal).
  `BUGS.md` → BUG-5.
- **`-setcookie` baked into `main.rs`.** The Erlang dist cookie is hardcoded in the
  kernel argv scaffolding (uncommitted in-tree). Should move to **boot.config
  (per-image)** so it's not a kernel constant and doesn't leak/collide across
  deployments. *Fix:* thread it through `tyn_boot`'s env like the other runtime
  config. (Currently held out of git deliberately — do not commit the scaffolding
  as-is.)
- **DB password drift — `tynpass123`, out-of-band, not in git.** The Postgres
  password used in TLS/dist testing lives only in operator memory / host state, not
  in any tracked config. *Bite:* a fresh operator can't reproduce the DB path.
  *Fix:* record provenance + where it's set; decide tracked-secret vs
  documented-external.
- **IAM: `tyn-build-role` is missing perms (a recurring gap-class).** Two instances
  hit so far: (1) no **`ec2:RevokeSecurityGroupIngress`**, so ≥2 in-VPC SG rules opened
  during testing (**9100** dist, **6432** TLS/pgbouncer) are **un-revokable** by the
  tooling and linger; (2) no **`iam:PassRole`** for `tyn-build-role`, so the build host
  **can't launch a fresh build instance *with* the `tyn-build-profile`** — a fresh host
  comes up with no aws access, which blocks it from self-serving the metal cut and
  forces keeping the old (role-bearing) host alive just for aws (paying double). *Fix:*
  one IAM policy addition on `tyn-build-role` grants the class — add `iam:PassRole` (on
  `tyn-build-role`) + `ec2:RevokeSecurityGroupIngress`; then a fresh build host is
  self-sufficient (attach the profile at launch) and stale SG rules can be revoked.
  Audit the role for the whole ec2/iam action set the build/deploy flow needs, once.

## Kernel hardening

- **Guard pages under stacks — the top hardening item.** Tyn identity-maps
  0–4 GiB, so a stack under/overflow **doesn't fault** — it silently corrupts
  adjacent mapped memory (the naive red-zone fix's `bad tag` crash was this). Guard
  pages under thread stacks would convert this whole class from silent corruption
  into a clean `#PF`. Reframed from nice-to-have to **the thing that would have made
  the red-zone class debuggable.** (`BUGS.md` → systemic hazard.)
- **4 GiB image/heap ceiling.** The identity map ends at exactly 4 GiB; large
  images/allocations near the top produce wild-pointer faults (BUG-4 class). Extend
  the map, or bound placement, if images grow.

## Dead code / cruft (proven, safe to remove)

- **`thread.rs` — the entire dead thread system.** `pub mod thread` with **no
  external callers** (`sys_clone` uses `sched::spawn`; `main.rs` runs `sched::init`).
  Its `CONTEXTS`, `KSTACK_NEXT`, `spawn`, `context_switch`, `yield` are all
  unreachable. Completeness-proven in `docs/STACK_ALLOCATOR_INVENTORY.md`. *Fix:*
  delete (removes a confusing second thread system + the `context_switch`-duplicate
  trap that misled the red-zone hunt for a session).
- **`syscall_stack_1` — dead 32 KiB static.** Declared (`syscall.rs:102-104`),
  never referenced. (Ironically useful: it's free space above `syscall_stack_0_top`
  that BUG-1's Path A can reuse for thread 0's preempt region — see the inventory.)
- **`percpu.rs::PerCpuData.kernel_stack` — unused field** (`[u8; 16384]` declared +
  zero-init'd, never read; TSS uses `ist_stack`, ring-0 never takes `rsp0`).
- **Arc scaffolding in-tree.** Diagnostic scaffolding built during the red-zone hunt
  (GPR fault dump, PREEMPT_DIV throttle) was reverted from `interrupts.rs`. Sweep
  for any remaining and loud-comment or remove.

## Latent correctness (cross-ref `BUGS.md`)

- **`arch_prctl(ARCH_SET_FS)` doesn't update `ctx.fs_base`.** Moot today (the switch
  reads live `rdmsr`), would bite if any path ever trusted the saved copy. `BUGS.md`.
- **`tyn_boot exit_group(127)` on a config value of `"0"` (BUG-2).** A literal `"0"`
  in boot.config kills boot — and it corrupted a *measurement* during the arc.
- **ERTS build can't link two static NIFs (BUG-3).** `--enable-static-nifs=a,b`
  mangles paths; worked around by one-module-many-functions. Tooling debt.

## Test / validation debt

- **Doc-status pass owed (Phase 1d).** The `docs/` + `directions/` corpus has grown
  large and some docs describe superseded states (wall_clock retraction, dist
  FAR→BUILDABLE, stunnel→pgbouncer). Label each current / superseded / paper-material
  so the corpus stops lying by ambiguity.
- **Standing suites not built yet (Phase 2).** Only the preemption probes are tracked;
  unit/resiliency/fuzz/soak layers are planned, not shipped
  (`directions/AUDIT_AND_TESTING_PLAN.md`). Networking claims must be measured on
  **Nitro, not QEMU** (QEMU has faked bottlenecks/truncation repeatedly).
