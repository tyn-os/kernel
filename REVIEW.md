# Code Review — 2026-04-26

## Summary
- Total files: 26 (.rs + .S)
- Total lines: 5,739
- Issues found: 8 critical, 14 warning, 12 minor

## Critical Issues

1. **`static mut` without locks (SMP data races)** — `PIPES` (pipe.rs), `OPEN_FILES`/`DIR_SLOTS` (vfs.rs), `SOCKETS` (net/socket.rs), `CONTEXTS` (thread.rs) are accessed from multiple CPUs without synchronization. Safety comments say "single-threaded" but SMP is active. Need spinlocks or `UnsafeCell` wrappers.

2. **`futex_wait` panics if `current` is None** — sched.rs:482 calls `CPU_QUEUES[cpu].current.unwrap()` which panics if called from idle context. Should return 0 or -EAGAIN instead.

3. **Thread ID overflow → OOB access** — sched.rs:257 `NEXT_TID.fetch_add(1, Relaxed)` without bounds check. If TID exceeds `MAX_THREADS`, the cast `tid as usize` indexes out of bounds.

4. **`sys_brk` accepts any address** — syscall.rs:608 sets `BRK_TOP` to any user-provided value without range validation. A userspace bug can point BRK into kernel memory.

5. **`SYS_NEWFSTATAT` wrong argument** — syscall.rs:376 passes `a2` as the stat buffer but the Linux ABI has `(dirfd, pathname, statbuf, flags)` where `a1=pathname, a2=statbuf`. The path argument is ignored.

6. **`find_socket` returns aliased `&'static mut`** — net/socket.rs:53 manufactures an unbounded mutable reference from `static mut`, allowing aliased mutable references if called concurrently.

7. **No pointer validation on syscall arguments** — Syscall handlers (read, write, mmap, sendmsg) dereference user-provided pointers without bounds checking. Safe only because all memory is identity-mapped, but a stale pointer to kernel data could corrupt state.

8. **VFS lseek wraps on negative offset** — vfs.rs:215 `(file.pos as i64 + offset) as usize` silently wraps to a huge value on negative seek, bypassing bounds.

## Warnings

1. **Hardcoded LAPIC address 0xFEE00020** — sched.rs:129, syscall.rs:71 — reads APIC ID from hardcoded MMIO address instead of using the ACPI-discovered `APIC_BASE`. Would break on non-standard hardware.

2. **`APIC_BASE`/`CALIBRATED_TICKS` are `static mut` without barriers** — apic.rs:29-31 — written on BSP, read on APs without memory fences. Works on x86 (TSO) but violates Rust memory model.

3. **Pipe lock bypass** — pipe.rs `set_nonblock`, `is_pipe_fd`, `any_has_data`, `has_data` access `PIPES` without `PIPE_LOCK`.

4. **No fd-type tracking** — syscall.rs:367 `sys_close` calls vfs::close, pipe::close, and socket::close on every close regardless of fd type. Masks leaks and wastes cycles.

5. **Magic fd ranges enforced by convention only** — pipes (200+), sockets (500+), VFS (1000+). No runtime collision check.

6. **Trampoline user stack write without guard** — interrupts.rs:116 writes to `user_rsp - 8` inside timer ISR with no underflow check. If RSP is near 0, this writes to unmapped memory.

7. **Lock ordering undocumented** — `FUTEX_LOCKS` → `THREAD_LOCK` in futex_wake, but no formal ordering comment. If any path reverses this, deadlock.

8. **`raw_hex` doesn't acquire serial lock** — serial.rs:41 — concurrent `raw_hex` and `_print` calls interleave bytes.

9. **Spawn ignores MAX_THREADS failure** — sched.rs (spawn silently returns if full); thread.rs:84 (same pattern).

10. **Auto-responder fd check is fragile** — syscall.rs:536 `fd >= 205 && fd % 2 == 1` assumes pipe numbering parity.

11. **`sys_sched_getaffinity` always returns 1-CPU mask** — syscall.rs:363 ignores actual CPU count.

12. **VFS→scheduler coupling** — vfs.rs:139 directly calls `sched::enable_blocking_futex()` on open #92.

13. **`sys_clone` ignores CLONE_THREAD/CLONE_SIGHAND flags** — syscall.rs:867 returns success regardless.

14. **Blocking futex threshold is a magic number** — vfs.rs:139 `n == 91` will break if boot-time opens change.

## Minor

1. **Stale comment** — heap.rs:8 says "64 KiB" but HEAP_SIZE is 2 MiB.
2. **Dead code** — syscall.rs:329 SC counter incremented then discarded (`let _ = c`).
3. **Dead code** — thread.rs:359 hardcoded address `0x4a955914` debug check.
4. **Debug logging in production** — pipe.rs:119 `if fd == 205`, sched.rs:415 YIELD_LOG, sched.rs:557 WAKE_LOG.
5. **Misleading name** — main.rs:70 `HELLO_ELF` includes `beam.smp.elf`.
6. **Bare syscall numbers** — syscall.rs:387-435 uses `267`, `213`, `186`, `96`, `19`, `270`, `319`, `435` instead of named constants.
7. **Duplicate constant** — syscall.rs:279 `SYS_CLOCK_GETTIME64 = 228` same value as `SYS_CLOCK_GETTIME`.
8. **Long function** — `syscall_dispatch_inner` is ~160 lines; inline implementations should delegate.
9. **RX/TX serial logging** — net/device.rs:69,121 logs every packet; floods output under traffic.
10. **`accept` allows CloseWait** — net/socket.rs:219 accepts half-closed connections.
11. **PCI panic** — main.rs:230 `expect` on `PciTransport::new`; missing device shouldn't be fatal.
12. **Missing doc comments** — Most public functions lack `///` documentation.

## Per-File Notes

### src/main.rs (263 lines)
- `HELLO_ELF` name misleading; panics on missing PCI device; memory layout constants undocumented.

### src/sched.rs (747 lines)
- Core scheduling logic sound after idle context fix. `static mut` arrays are the main SMP concern. TID overflow and current.unwrap() are crash risks. Debug logging should be removed.

### src/syscall.rs (1242 lines)
- Largest file. Dispatch logic is correct but inline implementations should be factored out. Bare syscall numbers need named constants. Pointer validation missing but safe due to identity mapping. NEWFSTATAT argument order wrong.

### src/interrupts.rs (205 lines)
- Trampoline preemption is architecturally sound. User stack write needs bounds check.

### src/pipe.rs (240 lines)
- Lock bypass in helper functions is the main SMP issue. Auto-responder is fragile.

### src/vfs.rs (445 lines)
- No locking on OPEN_FILES. Scheduler coupling via enable_blocking_futex. Magic threshold.

### src/net/socket.rs (565 lines)
- No lock on SOCKETS. Aliased mutable references. Missing bounds checks on setsockopt/getsockopt.

### src/thread.rs (404 lines)
- Legacy module from pre-SMP. `static mut CONTEXTS` without locks. Debug code at hardcoded addresses.

### src/serial.rs (111 lines)
- `raw_hex` inconsistency with locking. Otherwise clean.

### src/acpi.rs (179 lines), src/apic.rs (254 lines), src/percpu.rs (116 lines), src/smp.rs (179 lines)
- SMP infrastructure is solid. Minor: AP stack leak on timeout, hardcoded LAPIC address.

### src/elf.rs (175 lines), src/boot.rs (38 lines), src/multiboot.S (113 lines)
- Clean. ELF loader handles BSS zeroing correctly. Multiboot header depends on linker script symbols.

### src/memory/heap.rs (26 lines), src/drivers/virtio/hal.rs (52 lines)
- Minimal. Stale comment in heap.rs. DMA init edge case at address 0.

### src/net/mod.rs (100 lines), src/net/device.rs (135 lines), src/net/interface.rs (38 lines), src/net/tcp_echo.rs (73 lines)
- NET_LOCK exists but `is_initialized` bypasses it. Packet logging is noisy. tcp_echo unused in production.
