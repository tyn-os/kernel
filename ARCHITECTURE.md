# Tyn — architecture orientation

The front door. Short and stable; it points into the deep-dive docs rather than
inlining them. If you're new here, read this, then follow the pointers.

## What Tyn is

A **bare-metal x86-64 Rust microkernel that hosts an unmodified BEAM** (Erlang/OTP
27, the JIT `beam.smp`) — no Linux, no libc host. Tyn presents just enough of a
Linux-like syscall ABI for the stock, statically-linked musl `beam.smp` to run,
so real Elixir/Phoenix apps boot on it unchanged. ~9,600 LOC of kernel (`src/`).

**Layer model:**
1. **Boot** — multiboot2 → long mode → load `beam.smp` (embedded ELF) + the cpio
   rootfs (embedded / GRUB module) → jump to ERTS. (`docs/boot-flow.md`)
2. **Kernel** — the syscall ABI + subsystems below. Runs **ring 0** (BEAM too:
   `syscall` routes via LSTAR to `syscall_entry`, returns `jmp rcx`, no swapgs).
3. **ERTS/BEAM** — unmodified, in "user" space (identity-mapped, ring 0).
4. **App** — a stock `mix release` (Phoenix/Ecto/etc.), packed into the cpio.

Deep dives: **`docs/runtime-arch.md`** (runtime architecture),
**`docs/module-structure.md`** (module map), **`docs/TECHNICAL_REPORT.md`** (the
paper).

## The syscall surface — the curated set BEAM exercises

**53 handled syscalls** (`src/syscall.rs::syscall_dispatch_inner`) plus a small
socket-op range — *not* a general Linux surface, but exactly what a static musl
BEAM + Phoenix touches: process/threads (`clone`, `futex`, `sched_yield`,
`set_tid_address`, `rseq`), memory (`mmap`/`munmap`/`mprotect`/`brk`/`madvise`),
files/VFS (`openat`/`read`/`write`/`writev`/`fstat`/`sendfile`/`lseek`/`dup`),
net (a `socket`-op range → `src/net`), poll/epoll/timerfd, time
(`clock_gettime`/`nanosleep`), randomness (`getrandom`), TLS (`arch_prctl`).
**This bounded surface is a first-class property** — it's the paper's opening
contribution *and* the fuzzing target (small enough to fuzz meaningfully; see
`directions/AUDIT_AND_TESTING_PLAN.md` Phase 2c).

## Subsystem map

| Subsystem | Modules | Deep dive |
|---|---|---|
| Boot / ELF load | `boot.rs`, `multiboot.S`, `elf.rs`, `main.rs` | `docs/boot-flow.md`, `BOOT_RELIABILITY.md` |
| Scheduler / SMP / futex | `sched.rs`, `smp.rs`, `percpu.rs`, `thread.rs`†, `interrupts.rs` | `docs/FUTEX_PROTOCOL.md`, `docs/FUTEX_HISTORY.md` |
| Syscall ABI | `syscall.rs` | `docs/runtime-arch.md` |
| Memory / paging / identity-map | `main.rs` (4 GiB identity map), `mmap` in `syscall.rs` | `docs/STACK_ALLOCATOR_INVENTORY.md` |
| VFS / cpio / tmpfs | `vfs.rs`, `tmpfs.rs`, `pipe.rs` | (in `docs/runtime-arch.md`) |
| Net stack | `net/{device,interface,socket,pci_io,mod}.rs` (virtio-net / ENA + smoltcp) | `docs/INBOUND_THROUGHPUT.md`, `docs/SEND_CORRUPTION.md` |
| Crypto shim | RustCrypto static NIF (`beam-build/`) | `docs/CAPABILITY_MAP.md` |
| RTC / clock | `rtc.rs`, `apic.rs` (PIT/APIC timer) | `docs/WALL_CLOCK.md` |
| Randomness | `rng.rs` (getrandom) | — |
| BEAM host interface | `beam-build/` (build), `src/vfs.rs` (embedded cpio), `tyn_boot` | `docs/BUILDING_ERTS.md`, `MESSAGE_DELIVERY.md` |
| Distribution | (dist over the socket surface) | `docs/DIST_ACCEPT_HUNT.md` |

† `thread.rs` is **dead code** (superseded by `sched.rs`; proven in
`docs/STACK_ALLOCATOR_INVENTORY.md`) — kept pending deletion (paydown).

Build & deploy: **`BUILDING.md`**, **`docs/BUILDING_ERTS.md`**, **`docs/DEPLOY.md`**,
**`docs/CLEAN_CLONE_JOURNEY.md`**. Capability status: **`docs/CAPABILITY_MAP.md`**.
Open defects: **`BUGS.md`**. Regression suites: **`tests/README.md`**,
**`tests/simd/README.md`**.

## Honest limits

- **Ring-0 everything (confinement still open).** No user/kernel privilege
  separation; BEAM and kernel share ring 0, so a memory-safety bug in either is
  unconfined. Enforced confinement is the **Isolation arc**
  (`docs/ISOLATION_SCOPING.md`); Stage 0 (paging + stack guards, below) is the first
  step, but the ring-3 / US-page boundary that would actually *confine* BEAM is not
  yet built.
- **Paging + partial stack guards (Isolation Stage 0, done).** The boot 1 GiB
  huge-page map is replaced at boot by a **hybrid identity map** (`src/memory/paging.rs`):
  GiB 0 is a splittable **2 MiB/4 KiB** hierarchy; GiB 1–3 stay **1 GiB** huge pages
  (kept coarse — blanket 2 MiB cost a measured ~17% single-connection serving
  throughput, and only GiB 0 needs fine granularity today). **Guard pages are
  installed under the per-thread scheduler kernel stacks** (the `KSTACK` arena at
  `0x0700_0000`, `src/sched.rs`): an overflow there now takes a clean `#PF` at the
  guard instead of silently corrupting the neighbor. **Still unguarded:** the
  per-CPU stacks (`kernel_stack`/`ist_stack`, `src/percpu.rs`, heap-embedded) and
  thread 0's static `syscall_stack_0`. Everything stays identity-mapped and
  supervisor-only (no US/NX yet — that is a later Isolation stage). Still caps
  images/heap near the 4 GiB wall.
- **BUG-1 open.** The preemption trampoline clobbers the interrupted thread's SysV
  red zone → occasional wrong `:erlang.md5`/`binary.copy`. Root cause measured,
  correct fix (Path A) pending against `docs/STACK_ALLOCATOR_INVENTORY.md`.
- **Validation skew.** Much is proven on **QEMU/TCG only**; QEMU has repeatedly
  faked bottlenecks/truncation/crashes, so networking + timing claims are trusted
  only when measured on **Nitro** (real KVM). Standing rule.
- **Known-latent:** `arch_prctl` doesn't update `ctx.fs_base` (moot today);
  `-setcookie` baked in `main.rs` (should move to boot.config); `tyn_boot`
  `exit_group(127)` on a config value of `"0"` (BUG-2). See `BUGS.md`.
- **TCG boot `#PF`** on large images (BUG-4) — TCG-only so far, un-unified.

## Investigation trail vs current truth

`directions/*.md` (115+ files, gitignored) is the **hunt log** — many describe
states that later changed. When in doubt, this file + `BUGS.md` + the tracked
`docs/` are current truth; `directions/` is history. (A doc-status pass — labelling
each doc current / superseded / paper-material — is Phase 1d of the audit plan.)
