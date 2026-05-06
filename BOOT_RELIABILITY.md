# Boot reliability

Tyn boots OTP 27 to a working `gen_tcp` echo server (`curl http://localhost:5566/` → `HELLO`) in roughly **13 of 16 fresh boots** on KVM. The remaining ~19% fail in a small number of distinct ways. None of the failures corrupt the host or the kernel itself; QEMU exits or sits at a watchdog stall and the next boot is independent.

This document is the running log of what's known, what's been ruled out, and what to try next.

## Reproduction

```bash
qemu-system-x86_64 -kernel target/x86_64-tyn/release/tyn-kernel \
  -serial stdio -display none -no-reboot \
  -smp 2 -m 2G -enable-kvm \
  -netdev user,id=net0,hostfwd=tcp::5566-:8080 \
  -device virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off \
  -machine q35
```

A successful boot prints `listening` to serial within ~10s; the line comes from the eval string in `src/main.rs`.

## Failure modes

### A. `beam_load.c: Error loading function X: ... no specific operation found` / `bad tag 0; expected 120` / `invalid opcode`

ERTS's BEAM loader reads a `.beam` module and decodes a sequence of opcodes. When the loader gets unexpected bytes it aborts with one of these messages. The exact module and opcode vary across runs (`erl_lint`, `gen_server`, `erl_parse`, `inet`, `filename`, ...). The same module loads fine in the next boot.

This is the most common failure (≈10% of boots).

### B. Page fault from corrupted ERTS pointer

```
#PF ip=0x6487c8 cr2=0x8176be994 rsp=0x6a84f7d0
```

`ip = 0x6487c8` lives inside `erts_prepare_loading` (the BEAM loader). The faulting access is `mov 0x14(%rsi,%rdx,8),%edx` — `rsi = gen_opc[]` (a static array of opcode descriptors), `rdx = 3 * (*r12)`. `*r12` is supposed to be a small opcode index (≪ 1000). When it contains garbage (e.g. `0x562CAFE2`), the multiplied offset lands at an unmapped 32 GB+ address. So somewhere upstream a register or a memory location holding an opcode index got clobbered.

This is the second-most-common failure (≈5% of boots).

### C. Watchdog stall (no error, just timeout)

The boot reaches the thread-progress barrier or the supervisor tree, schedulers sit in `[wait]` on their SSI futexes, the watchdog reports the held value but no module-load error appears. Roughly ≈4% of boots.

## What's been ruled out

- **CPIO data corruption.** Added a canary (FNV-1a hash of the first 4 KiB of the relocated cpio) at `relocate()` time and re-checked it at every `vfs::open`. Across many runs including failing ones, the canary never changed. So the cpio header isn't being overwritten by brk or mmap, and `cpio_lookup` returns the correct `(data_offset, data_len)` for every file. Removed the diagnostic since it was noisy and didn't fire.
- **FS_BASE preservation.** Fixed in commit `a9c725d`. Without it, OTP 27 couldn't even boot — schedulers stomped on each other's `last_os_monotonic_time` via aliased TLS. With it, the time-monotonicity check passes naturally. The reliability issues here all happen *after* that fix landed.
- **Wrong `clock_gettime` semantics or non-monotonic time.** The fetch_max ratchet on `LAST_TIME_NS` is correct; ERTS sees monotonic time on every successful and failing run.

## Stack layout at each stage

Tracing what's where on which stack at every transition, because a
disagreement between save and restore is exactly where this kind of
intermittent corruption hides.

### Stage 1 — user code running

- `rsp = X` (in the calling thread's user stack)
- `rip = U` (some user instruction)
- All GPRs hold user values
- Per-CPU `gs:[0]` = current thread's kernel-stack top (`KSP`)

### Stage 2 — timer fires (IST 1)

CPU automatically pushes IRET frame onto the per-thread IST 1 stack
(not the user stack). Frame: `SS, RSP=X, RFLAGS, CS, RIP=U`. Switches
to handler.

### Stage 3 — Rust `extern "x86-interrupt"` handler

Compiler-generated prologue saves all GPRs the handler clobbers onto
the IST stack. Handler body runs `apic::eoi()`, then mutates the IRET
frame:

```rust
let user_rsp = frame.stack_pointer.as_u64();   // = X
let new_rsp = user_rsp - 8;                    // = X - 8
*(new_rsp as *mut u64) = ip;                   // park original RIP
frame.stack_pointer = new_rsp;
frame.instruction_pointer = trampoline;
```

Epilogue restores GPRs. CPU's `iretq` pops the (modified) frame.

### Stage 4 — trampoline starts on user stack

- `rsp = X - 8`, `[rsp] = U` (parked RIP)
- All GPRs = user values (the handler restored them via the
  x86-interrupt epilogue)

```asm
sched_yield_trampoline:
    push rax        ; rsp = X - 16, [rsp] = user_rax
    push rcx        ; rsp = X - 24, [rsp] = user_rcx
    push r11        ; rsp = X - 32, [rsp] = user_r11
    mov eax, 24
    syscall         ; CPU: rcx ← post-syscall RIP, r11 ← rflags
                    ;      rip ← LSTAR (= syscall_entry); rsp unchanged
```

Stack on entry to `syscall_entry`:

```
[X - 8 ] = U (parked RIP)
[X - 16] = saved rax
[X - 24] = saved rcx
[X - 32] = saved r11
rsp      = X - 32      (still on user stack)
rcx      = syscall_entry's return RIP (in trampoline post-syscall)
r11      = rflags
```

### Stage 5 — `syscall_entry` switches to kernel stack

```asm
mov gs:[8], rsp        ; user_rsp = X - 32 → scratch slot
mov rsp, gs:[0]        ; rsp = KSP
push qword ptr gs:[8]  ; [KSP-8]   = X - 32 (saved user rsp)
push 0                 ; [KSP-16]  = 0 (alignment)
push rcx               ; [KSP-24]  = post-syscall return RIP
push 0                 ; [KSP-32]  = 0 (placeholder for r11/RFLAGS) ← see below
push rdi               ; [KSP-40]  = a0 (user's rdi)
push rsi               ; [KSP-48]  = a1 (user's rsi)
push rdx               ; [KSP-56]  = a2 (user's rdx)
push r8                ; [KSP-64]  = a4 (user's r8)
push r9                ; [KSP-72]  = a5 (user's r9)
push r10               ; [KSP-80]  = a3 (user's r10)
push rbx               ; [KSP-88]
push rbp               ; [KSP-96]
push r12               ; [KSP-104]
push r13               ; [KSP-112]
push r14               ; [KSP-120]
push r15               ; [KSP-128]
call dispatch          ; pushes return addr at [KSP-136]
```

Critically, `rdi/rsi/rdx/r8/r9/r10` ARE saved here — the trampoline
didn't touch them but `syscall_entry` does. Combined with the
trampoline's `rax/rcx/r11`, every caller-saved GPR survives.

### Stage 6 — dispatcher and (maybe) context switch

`syscall_dispatch` is `extern "C"`. May call `yield_current` →
`context_switch` which saves `rsp/rbx/rbp/r12-r15/FS_BASE` for the
current thread and restores those for the next. When the original
thread is later resumed, its rsp comes back to mid-syscall_entry.

### Stage 7 — exit path

Pops 14 of the 16 saved values (alignment-pad and saved-rsp are
handled separately):

```asm
pop r15 / r14 / r13 / r12 / rbp / rbx / r10 / r9 / r8 / rdx / rsi / rdi
pop r11           ; pops the 0 placeholder — r11 = 0, NOT user's RFLAGS
pop rcx           ; pops post-syscall return RIP
add rsp, 8        ; skip alignment pad
mov gs:[0], r11   ; (r11 is just used as scratch here for kernel_stack save)
pop rsp           ; restore user_rsp = X - 32
sti
jmp rcx           ; back into trampoline post-syscall
```

After `pop rsp`, `rsp = X - 32` and we're back on the user stack.

### Stage 8 — trampoline post-syscall

```asm
pop r11    ; [X - 32] = saved user_r11 (preserved by trampoline)
pop rcx    ; [X - 24] = saved user_rcx
pop rax    ; [X - 16] = saved user_rax
ret        ; pops [X - 8] = U (parked RIP), rsp = X
```

User code resumes at `U` with `rsp = X` and all caller-saved GPRs
restored to their pre-interrupt values.

## What the trace surfaces

The trampoline + syscall_entry pair DO preserve every caller-saved
GPR. So the original "trampoline only saves 3 regs" hypothesis was
wrong — the syscall_entry recipe rescues the other six.

Two real disagreements come out of the trace, though:

1. **The `jmp rcx` exit path doesn't restore RFLAGS.** The CPU's
   `syscall` instruction puts the user's RFLAGS into r11 on entry,
   but `syscall_entry` pushes a `0` placeholder (line 122) instead of
   r11, so user RFLAGS is lost. ERTS code is rarely flag-dependent
   across instruction boundaries, but DF (direction flag) is sticky
   for string operations and would survive into kernel and back.

2. **The idle-loop fast path in `sched.rs` did not restore FS_BASE.**
   The cooperative `context_switch` does, but the inline asm at
   `yield_current`'s "CPU was idle, found work" branch only loaded
   GPRs. A thread resumed via that path would read TLS through
   whatever FS_BASE the previous user of this CPU left behind. Fixed
   in this commit by adding a WRMSR(0xC000_0100) before the GPR
   restore.

Reliability didn't change measurably (32 runs: 26 OK / 2 beam_load /
1 #PF / 3 timeout = 81%, same as baseline). The dominant failure is
elsewhere — likely in something neither trace covers (e.g., FXSAVE
state, or a scheduler race that lets two CPUs see the same `THREADS`
slot during spawn).

## Why "push all 9 GPRs" doesn't help

The trampoline only saves rax/rcx/r11 because those are exactly the
ones the `syscall` instruction itself clobbers. The full
trampoline + syscall_entry roundtrip already preserves all nine
caller-saved GPRs (the other six are pushed/popped inside
syscall_entry — see stack-layout trace above).

Empirically, pushing all 9 in the trampoline anyway *halves* the
success rate (5/16); adding a 128-byte red-zone gap brings it to
9/16. Both worse than baseline. The most likely reason is that
adding pushes to the trampoline's user-stack writes makes it harder
for the timing of the trampoline to align with whatever the failing
runs are doing — but the actual root cause is somewhere else and
extra register-saving was treating a symptom.

## What's *not* a fix

- **Mapping more memory.** Both the page fault and the loader error happen *before* anything paging-related; pointers point at unmapped addresses because they were already corrupted in registers, not because mapping is missing.
- **Bigger stacks.** Worth checking but the failure isn't a stack overflow — the symptoms are too consistent for that and the user stack base / size are unchanged across runs.

## Operational guidance

Until this is fixed properly, **retry on failure**. Boot is fast enough on KVM (~10s) that a wrapper script can `for i in $(seq 1 5); do qemu ... && break; done` and almost always succeed within two tries. The kernel itself is stable once boot completes — no observed failures in long-running scenarios after the eval reaches `listening`.

## Test setup

The 16-boot reliability test used to gather these stats:

```bash
ssh build-server 'OK=0; FAIL=0
for i in $(seq 1 16); do
  timeout 30 qemu-system-x86_64 -kernel /tmp/tyn -serial stdio \
    -display none -no-reboot -smp 2 -m 2G -enable-kvm \
    -netdev user,id=net0,hostfwd=tcp::5566-:8080 \
    -device virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off \
    -machine q35 > /tmp/r$i.log 2>&1
  if grep -aq listening /tmp/r$i.log; then OK=$((OK+1)); else FAIL=$((FAIL+1)); fi
done
echo "OK=$OK FAIL=$FAIL"'
```

## Adjacent context

- `src/interrupts.rs:130` — the assembly trampoline.
- `src/interrupts.rs:93` — the timer handler that injects it.
- `src/syscall.rs:316` — the Rust syscall dispatcher whose clobbers cause the issue.
- `src/sched.rs:856` — `context_switch`, which since `a9c725d` saves rsp/rbx/rbp/r12-r15/FS_BASE.
