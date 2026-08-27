# BEAM/kernel Isolation — Scoping

> Status: **scoping (design, no code)**. First deliverable of the Wedge-A isolation
> arc (per `WEDGE_DECIDED`). Grounded in a file:line audit of the current
> ring-0-everything architecture (see [Current state](#1-current-state-ring-0-everything),
> all cites verified against the tree). Companion to `ARCHITECTURE.md` §"Honest
> limits" (`:63-79`), which names ring-0-everything and no-guard-pages as the top
> hardening items this arc closes.

## 0. Why this is the #1 build

Wedge A is **security via a small TCB**. Today "small TCB" means *small codebase* —
not *enforced confinement*. Kernel and unmodified BEAM share **ring 0, one page
table, one flat identity map** (`ARCHITECTURE.md:64` "A memory-safety bug in either
is unconfined"). So the wedge's headline — a confined blast radius — is
**aspirational until this lands**. The README already carries the honest caveat
(security claim paired with "not a confined TCB"). Isolation is what converts the
claim from *small* to *enforced-small*.

Pivoting here now rests on a **measured-sound foundation**: the just-run Nitro
benchmark proved the primary serving path is production-viable (HTTP 13.3 MB/s,
HTTPS ~13 MB/s single-connection bulk, byte-exact). We are not building the
differentiator on an unverified base — and that 13 MB/s number becomes a hard
**regression baseline** the isolation boundary must not break.

## 1. Current state (ring-0-everything)

Everything the boundary must create is **absent today**; the transition *mechanism*
mostly exists. Precise starting point:

| Concern | Today | Cite |
|---|---|---|
| Privilege | BEAM runs in **ring 0**. `jump_to_user` is a plain `jmp {entry}` — no `iretq`, no CS/SS reload, no ring change (the name is aspirational). | `src/syscall.rs:2514-2529`, `src/main.rs:559` |
| Segments | GDT has **only DPL=0** code/data + TSS. No ring-3 segment exists. | `src/percpu.rs:49-53`, `src/multiboot.S:90-95` |
| Syscall entry | **Real `syscall`/LSTAR trap already built**: `syscall_entry` saves GPRs, loads kernel stack from `gs:[0]`, dispatches (~108 match arms / "53 handled" + socket range). musl-BEAM already funnels every syscall through it. | `src/syscall.rs:33-76, 115-226, 448` |
| Syscall return | `jmp rcx` — **not `sysretq`**, no `swapgs`, no ring change. | `src/syscall.rs:191-209` |
| Page tables | **One PML4**, built once in asm, never replaced. **4 GiB identity** via four 1 GiB huge pages (`0x83` = P\|W\|PS). **No US bit anywhere** — all supervisor. | `src/multiboot.S:98-107` |
| Paging layer | **None.** No `paging.rs`. `mmap`/`brk` are **bump allocators** over already-identity-mapped RAM; nothing sets PTE flags post-boot. | `src/syscall.rs:331-332, 1104-1136` |
| Kernel heap | 16 MiB `static HEAP` + `linked_list_allocator`. **Socket buffers live here** (grown 2→16 MiB because they exhausted it). | `src/memory/heap.rs:8-56` |
| BEAM regions | ELF copy `0x1400_0000`; JIT/`mmap` `0x1A00_0000`–`0xA000_0000` (BeamAsm emits native code here); user stack `0x0E00_0000` (2 MiB). All supervisor, all reachable from the kernel and vice-versa. | `src/main.rs:207,295-297`, `src/syscall.rs:331` |
| Interrupts | One shared IDT. Timer(32)+IPI(34) share **IST0** (16 KiB per-CPU). **No ring change on interrupt** (CPL already 0). Fault handlers **halt** (no demand paging). | `src/interrupts.rs:9-45, 206-290`, `src/percpu.rs:20-47` |
| Preemption | BUG-1 Path A trampoline **rewrites the IRET frame** to yield; assumes the timer frame has **no SS:RSP** (same-ring interrupt). | `src/interrupts.rs:104-204` |
| Guard pages | **None.** Stack over/underflow silently corrupts the neighbor (256 B slack, not a guard). Named the "top hardening item." | `ARCHITECTURE.md:66-69`, `src/sched.rs:438-443` |

**Two facts make this tractable rather than a rewrite:**

1. **The syscall trap already exists and BEAM already uses it.** We are adding the
   *privilege* to an existing *mechanism*, not building a syscall layer. The ~53
   handlers already parse user args — they just currently trust them (ring-0
   pointers).
2. **Unmodified musl-BEAM is already ring-3 Linux userland by construction.** It
   expects to run unprivileged and reach the OS only through `syscall`. It executes
   no privileged instructions in the normal path (it's a Linux binary). Tyn runs it
   in ring 0 today purely as a bring-up shortcut. **Isolation moves BEAM to where it
   already assumes it is** — which bounds the surprise surface to Tyn's own glue, not
   ERTS behavior. (This assumption must still be *audited*, §7, but it is the
   difference between "reconfigure the CPU" and "port BEAM".)

## 2. What "isolation" means for Tyn (threat model)

**In scope (v1 confinement claim):** a **memory-safety fault in BEAM cannot reach
kernel memory or control flow.** A wild write/read/exec from the BEAM side (a NIF
bug, a JIT miscompile, a corrupted process heap) must **fault and be contained** —
the offending scheduler thread is stopped, the kernel survives and keeps serving.
The TCB = the kernel (ring 0, supervisor pages). Everything in the BEAM address
space (ERTS, the JIT, all NIFs incl. rustls/crypto, all Erlang processes) is the
**untrusted payload** behind the boundary.

**Out of scope (named, deferred):**
- **Mutually-distrusting Erlang processes.** v1 confines BEAM-as-a-whole from the
  kernel, not process-from-process inside BEAM (that's the BEAM's own job / a later
  step).
- **Speculative side channels (Meltdown/Spectre).** A shared-PML4 design (§4, Stage 3)
  leaves the kernel mapped (US=0) in BEAM's address space — SMAP/US block
  *architectural* access but not speculative reads. True defense needs separate page
  tables / KPTI (Stage 4) — deferred unless the threat model demands it.
- **Malicious (not buggy) native code that executes privileged instructions.** A NIF
  *could* try `cli`/`wrmsr`/port I/O; in ring 3 those `#GP` and are contained — but a
  NIF is already arbitrary native code in the payload, so this is a hardening
  boundary, not a claim of NIF sandboxing.

## 3. The three sub-problems

Isolation is three separable pieces. They compose, but each has independent value
and its own teeth.

### 3a. Privilege separation (ring 3)
Add DPL=3 code/data segments; set `TSS.RSP0` so a ring-3→0 transition lands on a
known kernel stack; add `swapgs` on syscall/interrupt **entry and exit** (kernel GS
vs user GS); return to BEAM via `iretq` (initial entry) and `sysretq` (syscall
return) instead of `jmp`. The existing `syscall_entry`/`SFMASK`/LSTAR plumbing
stays; it gains a real CPL change.

### 3b. Address-space separation (page tables)
Build the **paging layer that does not exist today**. Split the 1 GiB huge pages
into a hierarchy that can set per-page **US** (user) and **NX** bits: BEAM regions
US=1, kernel regions US=0. Enable **SMEP** (kernel can't *execute* user pages) and
**SMAP** (kernel can't *read/write* user pages except via explicit `stac`/`clac`
windows). This is what actually enforces 3a's boundary in memory.

### 3c. The syscall/transition surface becomes a trust boundary
The ~53 syscalls currently **trust every pointer** (ring-0 caller). Each user
pointer crossing the boundary must be **bounds-checked to the US region and
copied** (`copy_from_user`/`copy_to_user`) — the kernel must never dereference a
raw BEAM pointer under SMAP. This is the largest *net-new* code surface and the
place a confinement bug would hide.

## 4. Shared-region handling (the hard part)

Each fixed-address region gets a side of the boundary:

| Region | Side | Note |
|---|---|---|
| BEAM ELF image, process heap, user stack | **US=1** (payload) | BEAM-owned, read/write/exec as today |
| JIT / `mmap` region (`0x1A00_0000`+) | **US=1, RWX** | BeamAsm writes native code then executes it in-place → the region is **user-RWX** (W^X would break the JIT; it never toggles perms — a *named* reduced-hardening spot, acceptable: it is BEAM's own code) |
| Kernel text/BSS, 16 MiB heap, kernel stacks, per-CPU GDT/TSS/IST, IDT | **US=0** (TCB) | supervisor-only; SMEP/SMAP enforce |
| **Socket buffers** (on the kernel heap) | **US=0** | BEAM cannot read them directly. Data already crosses via a **copy at the `read`/`write` syscall** — so the natural `copy_to_user`/`copy_from_user` point already exists. This is also where the dist-vs-HTTP receive-delivery cost lives; adding SMAP `stac`/`clac` + bounds-check here must not regress the 13 MB/s serving path. |
| cpio rootfs / VFS | **US=0** | BEAM reads files via syscalls (copy), never direct |

The uncomfortable one: **socket buffers on the kernel heap**. A confined BEAM must
reach them only through the copy at the syscall boundary — which is already how
`read`/`write` deliver bytes. So no data-path *re-architecture* is needed, only
(a) mark the heap US=0, (b) validate+copy the user pointer. The cost is one SMAP
window per syscall on the hot serving path — **measure it against 13 MB/s.**

## 5. Guard pages — the smaller sibling (and a prerequisite)

Guard pages (unmapped pages under kernel stacks so overflow **faults** instead of
silently corrupting the neighbor — `ARCHITECTURE.md:66-69`) are **hardening *within*
an address space**, independent of ring separation. But they are not merely a
sibling — they are a **stepping stone**: setting a page not-present under a stack
requires the **same per-page paging layer** that US/NX bits need (§3b). Today's
1 GiB huge pages can't express either. So:

- **Building guard pages builds the paging machinery Stage 2/3 depends on**, at low
  risk, with immediate independent value (closes the top hardening item; also
  **unblocks in-situ syscall fuzzing**, which needs fault-isolated stacks).
- Recommended as **Stage 0** — it de-risks the paging layer before any privilege
  change rides on it.

## 6. Incremental staging (verifiable, not big-bang)

Each stage boots, re-runs the regression baselines (§8), and is independently
valuable. No stage requires the next.

- **Stage 0 — Paging layer + guard pages.** Replace the 1 GiB huge-page map with a
  2 MiB/4 KiB hierarchy; unmap a page under each kernel stack. *Teeth:* a deliberate
  kernel stack overflow **faults** (was: silent corruption). Delivers the machinery
  §3b needs. Independent win.
- **Stage 1 — US/NX *attribution* only (still ring 0). [DONE]** Mark BEAM regions
  US=1, kernel US=0, NX on non-exec data (`src/memory/paging.rs::attribute_regions`).
  **Do NOT enable SMEP/SMAP here** — they gate on CPL, and with BEAM still at ring 0
  they would fault on BEAM's *own* execution/data access (SMEP on US=1 code fetch,
  SMAP on US=1 data read). SMEP/SMAP + the SMAP-driven copy-site *discovery* are only
  clean once BEAM is at ring 3 (Stage 3). So Stage 1 is pure attribution — **advisory
  and behaviorally inert** (proven: boots + serves byte-exact, throughput unchanged),
  labeling the map for Stage-3 enforcement. It also produces a **static copy-site
  draft** (`docs/ISOLATION_COPY_SITES.md`) by reading the ~53 handlers — the starting
  list Stage 2/3 build against, to be SMAP-validated-complete at Stage 3.
- **Stage 2 — Ring-3 transition plumbing (no BEAM yet).** DPL=3 segments, `TSS.RSP0`,
  `swapgs` entry/exit, `sysretq`/`iretq` return. Drive a **trivial ring-3 test shim**
  (not BEAM): it makes a few syscalls and then deliberately faults. *Teeth:* syscalls
  trap ring3→0 correctly; a ring-3 fault is **contained** (handler stops the shim,
  kernel survives). The confinement enforcement point (today's halting `#PF` handler)
  is reworked here to *contain* rather than *halt* for ring-3 faults.
- **Stage 3 — Move BEAM to ring 3.** Enter BEAM via `iretq` to ring 3; convert the
  syscall return path to `sysretq`+`swapgs`; **enable SMEP/SMAP** — now meaningful,
  since CPL distinguishes kernel (0) from BEAM (3) for free (no ring-0 AC-inversion).
  Add `copy_from_user`/`copy_to_user` + pointer bounds-checks across the ~53 syscalls;
  the **SMAP fault-hunt under exercised load (serving + dist + file I/O) validates and
  completes the Stage-1 static copy-site draft** (`docs/ISOLATION_COPY_SITES.md`) — any
  site SMAP faults on that the draft lacked is a caught gap; the empirical-completeness
  guarantee is earned here. **Rework the BUG-1 preemption trampoline** for a
  CPL-changing timer interrupt (the frame now carries SS:RSP, and `TSS.RSP0` gives a
  clean kernel stack — this may *simplify* BUG-1, see §7). Re-validate **all**
  baselines. This is the stage that realizes the wedge claim.
- **Stage 4 — Separate page tables (optional, later).** Per-domain PML4 + CR3 switch
  on syscall for address-space separation / speculative-channel defense. Costs a CR3
  reload + TLB per syscall — **only if** the threat model needs it and **only if**
  measured against 13 MB/s. US+SMAP (Stage 3) already enforce the v1 confinement
  claim, so this is a stronger-guarantee add-on, not a requirement.

## 7. Risks & open questions

- **Does unmodified musl-BEAM ever execute a privileged instruction?** Expected
  *no* (Linux-userland binary). **Audit** in Stage 2/3 by trapping+logging any ring-3
  `#GP`. If some path does (unlikely: e.g. an ERTS `rdtsc`/`cpuid` assumption — note
  `rdtsc` is legal in ring 3 with `CR4.TSD=0`, `src/main.rs:147`), it becomes an
  emulated trap. This is the single biggest feasibility unknown; the audit collapses
  it cheaply.
- **BUG-1 interaction (direct).** The preemption trampoline (`src/interrupts.rs:104-204`)
  assumes a same-ring timer frame (no SS:RSP) and rewrites it. Ring-3 preemption
  changes the frame shape — the trampoline **must** be reworked. Upside: a
  CPL-changing interrupt gets a clean kernel stack via `TSS.RSP0`, so the red-zone
  clobber that *is* BUG-1 may **dissolve** rather than merely move. Sequence Stage 3
  to co-resolve BUG-1 (and the related GP_HUNT #72 SMP red-zone class).
- **JIT is user-RWX** (§4) — a permanent reduced-hardening spot inside the payload.
  Acceptable (it is BEAM's own code, already trusted-as-payload), but named so it's
  not mistaken for an oversight.
- **Performance across the boundary.** Every syscall gains `swapgs`×2 + SMAP window +
  pointer copy (+ CR3 in Stage 4). The hot serving path (13 MB/s) and the dist path
  (already receive-delivery-bound at ~400 KB/s) must be **re-measured on Nitro** — the
  boundary must not turn 13 MB/s into the next 400 KB/s.
- **SMP.** Every core needs DPL=3 segments, `TSS.RSP0`, syscall MSRs, and the reworked
  trampoline. Per-CPU infrastructure already exists (`src/percpu.rs`), so this extends
  cleanly — but the confinement test must run **under `-smp`** (the class of bug this
  wedge exists to contain is exactly the SMP memory-corruption class).

## 8. Verification — teeth

**Confinement (the claim itself), proven by trying to violate it:**
- A test payload (a NIF or the Stage-2 ring-3 shim) that **writes** a kernel address
  (16 MiB heap / kernel text) → must `#PF` and be **contained**: offending scheduler
  thread stopped, kernel stays up **and keeps serving** (assert `/health` after).
- Same for **read** (SMAP) and **execute** (SMEP: jump into a US page from ring 0
  must fault). Run all three **under SMP**.
- A confinement failure = the kernel corrupts/halts. A pass = the kernel serves
  through the attack. This is the wedge's headline, measured, not asserted.

**Regression baselines — re-run every stage (must not regress):**
- Boot reliability (Nitro + KVM).
- **Serving throughput: HTTP 13.3 MB/s / HTTPS ~13 MB/s** single-connection bulk
  (the `tests/net_throughput` harness).
- md5-SMP correctness amplifier (the BUG-1 suite) + futex/SMP suites.
- dist ladder still forms + roundtrips (dist stays ~400 KB/s — not made worse).

## 9. Recommendation

Start with **Stage 0 (paging layer + guard pages)**: highest independent value
(closes the top hardening item, unblocks fuzzing), lowest risk, and it **builds the
per-page paging machinery every later stage needs** — so it de-risks the whole arc
before any privilege change rides on it. Then Stage 1 (US/NX + SMEP/SMAP map) proves
the map is correct while still ring 0. Stage 2 proves the ring-3 transition + the
confinement teeth on a trivial shim. Stage 3 moves BEAM across and co-resolves BUG-1.
Stage 4 stays deferred behind a threat-model + performance trigger.

Each stage: boot, re-measure the baselines, prove its teeth. No big-bang. No commits
until asked.
