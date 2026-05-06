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

## Hypothesis (incomplete)

The preemption trampoline `sched_yield_trampoline` in `src/interrupts.rs` injects a `syscall(SYS_sched_yield)` into a user thread that was interrupted mid-instruction. It currently saves only `rax`, `rcx`, `r11`:

```asm
sched_yield_trampoline:
    push rax
    push rcx
    push r11
    mov eax, 24
    syscall
    pop r11
    pop rcx
    pop rax
    ret
```

The `syscall` path enters our Rust syscall dispatcher, which is `extern "C"` — by the System V x86-64 ABI it's allowed to clobber every caller-saved register: `rax`, `rcx`, `rdx`, `rsi`, `rdi`, `r8`, `r9`, `r10`, `r11`, plus `xmm0..15`. So when the trampoline returns to user code, six GPRs (and the SSE state) may be different from what user code was holding when the timer fired. That fits both failure modes: a corrupted register in the BEAM loader's hot loop becomes either a bogus opcode index (loader error) or a bogus pointer (page fault).

## Why the obvious fix regresses

The obvious fix — push all nine caller-saved GPRs — *halves* the success rate (5/16). Adding a `sub rsp, 128` before the pushes to step past the SysV red zone (and an `add rsp, 128` before the final `ret` to re-find the parked RIP) brings it back to 9/16, still worse than the 13/16 baseline.

A few candidate explanations, none confirmed:

1. **Preemption count.** A larger trampoline does more work per timer tick. If preemption fires 100×/s during boot, the extra 18 instructions per fire is noise — but if some failing runs are timing-sensitive (e.g. a scheduler races to set TSE_SLEEPING before a wake clears it), the extra cycles may shift the race window the wrong way.
2. **Stack overflow.** The 9-push variant uses 80 bytes of user stack instead of 32. Most ERTS thread stacks have plenty of room, but if any worker hits a low-stack moment during boot, the larger save may push past the bottom.
3. **Hidden assumption.** Some part of the kernel may rely on a *specific* set of caller-saved regs being what they were before the trampoline. The 3-push version preserves rax/rcx/r11 specifically because the `syscall` instruction clobbers them; everything else is "officially" volatile per ABI but might be load-bearing in practice.

The right answer probably involves auditing the syscall dispatcher in `src/syscall.rs` to confirm what it clobbers, and possibly switching to a per-CPU scratch save area (FXSAVE-style) instead of pushing onto the user stack.

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
