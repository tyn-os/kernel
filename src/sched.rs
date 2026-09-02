//! SMP-aware thread scheduler.
//!
//! Each CPU has its own run queue. Threads are created on the BSP's queue
//! and may be moved between CPUs by futex_wake. The timer interrupt on
//! each CPU preempts the current thread.
//!
//! Key SMP invariant: futex_wait is atomic (check value + sleep under a
//! per-address spinlock) so that futex_wake can't race between the check
//! and the sleep.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use spin::Mutex;

use crate::serial_println;

const MAX_THREADS: usize = 32;
const MAX_CPUS: usize = 16;
const FUTEX_BUCKETS: usize = 16;

/// Thread state
#[derive(Clone, Copy, PartialEq)]
enum State {
    Ready,
    Running,
    Blocked, // waiting on futex
    Dead,
}

/// Saved thread context for context switching.
/// 16-byte aligned because FXSAVE/FXRSTOR require their target buffer
/// to be 16-byte aligned and `fxsave_area` lives at offset 64.
#[repr(C, align(16))]
struct ThreadCtx {
    rsp: u64,    //  0
    rbx: u64,    //  8
    rbp: u64,    // 16
    r12: u64,    // 24
    r13: u64,    // 32
    r14: u64,    // 40
    r15: u64,    // 48
    fs_base: u64, // 56  — TLS pointer; restored via WRMSR(0xC000_0100)
    fxsave_area: [u8; 512], // 64..576 — FPU/SSE state; FXSAVE/FXRSTOR target
}

/// 16-byte-aligned 512-byte buffer for FXSAVE/FXRSTOR.
#[repr(C, align(16))]
struct FxsaveBuf([u8; 512]);

/// Default FPU/SSE state, captured at kernel boot via FXSAVE on a freshly-
/// initialized FPU. Copied into every newly-created ThreadCtx so the first
/// FXRSTOR loads valid FCW/MXCSR (zero-initialized memory would set FCW=0,
/// which unmasks every FP exception and breaks ERTS).
static mut FXSAVE_TEMPLATE: FxsaveBuf = FxsaveBuf([0u8; 512]);

/// Capture the current FPU/SSE state as the template for new threads.
/// Must be called once during boot, before any thread is spawned.
pub fn init_fxsave_template() {
    unsafe {
        // Reset FPU to defaults, set MXCSR to its post-reset value.
        core::arch::asm!(
            "fninit",
            "mov dword ptr [rsp - 8], 0x1F80",
            "ldmxcsr [rsp - 8]",
            options(nostack),
        );
        let ptr = &raw mut FXSAVE_TEMPLATE as *mut FxsaveBuf as *mut u8;
        core::arch::asm!(
            "fxsave64 [{}]",
            in(reg) ptr,
            options(nostack, preserves_flags),
        );
    }
}

fn fxsave_template() -> [u8; 512] {
    unsafe { FXSAVE_TEMPLATE.0 }
}

/// Thread control block.
struct Thread {
    tid: u32,
    state: State,
    ctx: ThreadCtx,
    kernel_stack_top: u64,
    user_stack: u64,
    fn_ptr: u64,
    tls: u64,
    child_tid_ptr: u64,
    futex_addr: u64,   // address being waited on (if Blocked)
    futex_val: u32,     // expected value (if Blocked)
    in_idle_ctx: bool, // true if blocked via idle context (don't add to queue on wake)
    clone_r9: u64,     // saved R9 for child (musl's fn pointer)
    clone_rip: u64,    // saved return RIP for child
    home_cpu: u32,     // CPU where this thread was created (futex_wake targets this)
    wait_deadline_ns: u64, // 0 = no deadline; else monotonic_ns deadline for timed wait
    wait_timed_out: bool,  // set true by watchdog when deadline reached before wake
    /// monotonic_ns when this thread most recently transitioned to Blocked.
    /// 0 when not Blocked. The watchdog uses this as a stall safety net:
    /// any thread still Blocked on an infinite wait (`wait_deadline_ns == 0`)
    /// for longer than `BLOCKED_RESCUE_NS` is force-rescued as a spurious
    /// wakeup. See `watchdog_wake` and directions/BOOT_STALL_TSE.md.
    blocked_since_ns: u64,
}

/// Per-CPU run queue.
struct CpuQueue {
    current: Option<u32>,  // TID of currently running thread
    queue: VecDeque<u32>,  // TIDs of ready threads
    idle: bool,
}

// --- Global state ---

static mut THREADS: [Option<Thread>; MAX_THREADS] = {
    const NONE: Option<Thread> = None;
    [NONE; MAX_THREADS]
};
static THREAD_LOCK: Mutex<()> = Mutex::new(());
static NEXT_TID: AtomicU32 = AtomicU32::new(1); // 0 = main thread

static mut CPU_QUEUES: [CpuQueue; MAX_CPUS] = {
    const EMPTY: CpuQueue = CpuQueue {
        current: None,
        queue: VecDeque::new(),
        idle: true,
    };
    [EMPTY; MAX_CPUS]
};
/// Per-CPU idle context — used as a context_switch target when a thread
/// blocks and there's no other thread on the CPU. This allows the blocked
/// thread's register state to be properly saved in its ctx, so that
/// futex_wake can safely resume it on any CPU.
static mut IDLE_CTX: [ThreadCtx; MAX_CPUS] = {
    const EMPTY: ThreadCtx = ThreadCtx {
        rsp: 0, rbx: 0, rbp: 0, r12: 0, r13: 0, r14: 0, r15: 0, fs_base: 0,
        fxsave_area: [0u8; 512],
    };
    [EMPTY; MAX_CPUS]
};
/// Per-CPU idle stacks (4 KiB each).
static mut IDLE_STACKS: [[u8; 4096]; MAX_CPUS] = [[0; 4096]; MAX_CPUS];
/// Per-CPU: TID of the thread that context_switched to idle. Set before
/// context_switch, read by the idle loop to know which thread to check.
static mut IDLE_BLOCKED_TID: [usize; MAX_CPUS] = [0; MAX_CPUS];

static CPU_QUEUE_LOCKS: [Mutex<()>; MAX_CPUS] = {
    const M: Mutex<()> = Mutex::new(());
    [M; MAX_CPUS]
};

/// Per-futex-address spinlocks for atomic check-and-sleep.
/// Hash the address to a bucket.
static FUTEX_LOCKS: [Mutex<()>; FUTEX_BUCKETS] = {
    const M: Mutex<()> = Mutex::new(());
    [M; FUTEX_BUCKETS]
};

/// Pending wakes per bucket — set of addresses that received a futex_wake
/// while no waiter was sleeping. The next futex_wait at one of these
/// addresses consumes the pending wake and returns immediately, even if
/// the futex value matches the expected value.
///
/// This is required for ERTS's TSE event protocol where the waker can call
/// erts_tse_set (wake) BEFORE the waiter has entered erts_tse_wait. Without
/// pending wakes, the signal is lost: the wake arrives at an empty queue,
/// the waiter then resets the event value and blocks expecting a future
/// wake that was already issued.
///
/// One-shot semantics: a pending wake is consumed by the FIRST matching wait.
const PENDING_WAKES_PER_BUCKET: usize = 8;
struct PendingWakes {
    addrs: [u64; PENDING_WAKES_PER_BUCKET], // 0 = empty slot
}
static mut PENDING_WAKES: [PendingWakes; FUTEX_BUCKETS] = {
    const E: PendingWakes = PendingWakes { addrs: [0; PENDING_WAKES_PER_BUCKET] };
    [E; FUTEX_BUCKETS]
};

/// Insert a pending wake for `addr` in `bucket`. Caller must hold the bucket lock.
unsafe fn pending_wake_insert(bucket: usize, addr: u64) {
    let pw = &mut PENDING_WAKES[bucket];
    // If already present, no need to add (one-shot)
    for slot in pw.addrs.iter() {
        if *slot == addr { return; }
    }
    for slot in pw.addrs.iter_mut() {
        if *slot == 0 { *slot = addr; return; }
    }
    // Table full — drop the wake (rare; ERTS uses few addresses).
}

/// Try to consume a pending wake for `addr` in `bucket`.
/// Returns true if a pending wake existed and was consumed.
/// Caller must hold the bucket lock.
unsafe fn pending_wake_consume(bucket: usize, addr: u64) -> bool {
    let pw = &mut PENDING_WAKES[bucket];
    for slot in pw.addrs.iter_mut() {
        if *slot == addr { *slot = 0; return true; }
    }
    false
}

/// Per-CPU "lock to release after context_switch returns to a new thread".
/// The thread going to sleep holds the futex bucket lock across the switch;
/// the next thread that runs on this CPU (after context_switch returns) is
/// responsible for releasing the lock. -1 = no pending unlock.
/// This eliminates the wake-loss race where futex_wake fires after the lock
/// is dropped but before the waiter has actually entered sleep state.
static PENDING_UNLOCK_BUCKET: [core::sync::atomic::AtomicI32; MAX_CPUS] = {
    const M: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(-1);
    [M; MAX_CPUS]
};

/// Release a deferred futex bucket lock if one is pending on this CPU.
/// Called AFTER context_switch returns (we're running as a different thread
/// or resumed after a wake).
#[inline]
fn release_pending_unlock(cpu: usize) {
    let b = PENDING_UNLOCK_BUCKET[cpu].swap(-1, Ordering::Release);
    if b >= 0 {
        unsafe { FUTEX_LOCKS[b as usize].force_unlock(); }
    }
}

static NUM_CPUS: AtomicUsize = AtomicUsize::new(1);

pub fn num_cpus() -> usize {
    NUM_CPUS.load(Ordering::Relaxed)
}

/// Conservative futex valve. `false` = `futex_wait` spin-yields (never really
/// blocks); `true` = real blocking with `HLT` idle. It starts `false` so the
/// ENTIRE ERTS/app init window runs under spin-yield — this avoids a rare
/// (~3%) cold-boot deadlock in an init-time thread-progress wait that real
/// blocking exposes (root cause in docs/FUTEX_HISTORY.md; spin-yield eliminated
/// it 0/32 on the GCC-14 amplifier). It is flipped to `true` by
/// `enable_blocking_futex()` on the boot harness's `serial_shell ready` marker
/// (syscall.rs), an observable "the app finished booting" signal, deliberately
/// LATE and past the whole deadlock window. **Do not arm this earlier**:
/// open-count, `managed_count`, and listen-port triggers all proved to fire
/// *before* the deadlock and reintroduced the stall; the exact init edge is
/// unpinned, so the trigger is conservative by design.
static FUTEX_BLOCKING: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Arm real blocking. The PRIMARY caller is the `serial_shell ready` marker in
/// `syscall.rs` (the boot harness's app-is-up signal). **That trigger is a
/// tyn_boot.erl print, NOT a property of ERTS** — so if the boot script is
/// reordered, the marker text changes, or an app fails before `apply_config`
/// succeeds, this may never fire. The `watchdog_wake` elapsed-time backstop
/// arms it anyway after `BLOCKING_ARM_FALLBACK_NS` so no boot path spins
/// forever. Idempotent; safe to call from either site.
pub fn enable_blocking_futex() {
    if !FUTEX_BLOCKING.swap(true, core::sync::atomic::Ordering::Release) {
        crate::serial_println!("[sched] blocking futex enabled");
    }
}

/// Monotonic-ns timestamp of the first real spin-yield (set once in
/// `futex_wait_until`). 0 = none yet. The watchdog uses it to arm blocking
/// after a generous bound if the `serial_shell ready` marker never arrives.
static FIRST_SPINYIELD_NS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
/// Belt-and-braces: if the boot marker never prints, arm blocking this long
/// after the first spin-yield so idle CPUs eventually reach HLT instead of
/// spinning forever. Generous — well past a healthy boot (~10–60 s under TCG),
/// so it never pre-empts the real marker on a normal boot.
const BLOCKING_ARM_FALLBACK_NS: u64 = 120_000_000_000; // 120 s

fn futex_bucket(addr: u64) -> usize {
    (addr as usize / 4) % FUTEX_BUCKETS
}

/// Which CPU is the caller on? Read from the Local APIC ID register.
fn current_cpu() -> u32 {
    unsafe {
        let apic_id = *((0xFEE0_0020u64) as *const u32) >> 24;
        apic_id
    }
}

// --- Public API ---

/// Initialize the scheduler. Call once on BSP.
/// Per-CPU idle loop: HLTs until the blocked thread is woken or new
/// threads arrive in the queue, then handles them.
extern "C" fn cpu_idle_loop() -> ! {
    let cpu = current_cpu() as usize;
    loop {
        // We may have just been context_switched to from a futex_wait.
        // Release any pending bucket lock the previous thread handed off.
        // Must be at the TOP of the loop, not before, because every
        // context_switch back here may hand off a fresh lock.
        release_pending_unlock(cpu);

        // Drain any rescue requests the timer-context watchdog set
        // since we last looked. We hold no locks here, so it's safe
        // to take the futex / thread / queue locks the rescue needs.
        process_rescues();

        // Check-then-sleep, race-free. Disable interrupts, re-check the run
        // queue, and only HLT if it is still empty — `enable_and_hlt` makes the
        // `sti; hlt` pair atomic against a wake IPI landing in the window (the
        // sti-shadow defers the interrupt until after hlt). The 100 Hz timer
        // papers over a naive check-then-sleep within ≤10 ms today; this closes
        // the window outright so it can't degrade into a hang if the timer
        // discipline ever changes.
        x86_64::instructions::interrupts::disable();
        let empty = {
            let _qlock = CPU_QUEUE_LOCKS[cpu].lock();
            unsafe { CPU_QUEUES[cpu].queue.is_empty() }
        };
        if empty {
            x86_64::instructions::interrupts::enable_and_hlt();
        } else {
            x86_64::instructions::interrupts::enable();
        }

        // Unified resume: a woken thread is ALWAYS in the run queue.
        // futex_wake pushes to queue regardless of in_idle_ctx state.
        // No side channel — single source of truth for what to run next.
        let next = {
            let _qlock = CPU_QUEUE_LOCKS[cpu].lock();
            unsafe { CPU_QUEUES[cpu].queue.pop_front() }
        };
        if let Some(next_tid) = next {
            let _qlock = CPU_QUEUE_LOCKS[cpu].lock();
            unsafe {
                CPU_QUEUES[cpu].current = Some(next_tid);
                CPU_QUEUES[cpu].idle = false;
            }
            let next_idx = next_tid as usize;
            if let Some(next) = unsafe { THREADS[next_idx].as_ref() } {
                unsafe {
                    crate::syscall::set_current_kernel_stack(next.kernel_stack_top);
                    drop(_qlock);
                    // Switch to the new thread. When it yields, we return here.
                    context_switch(
                        &raw mut IDLE_CTX[cpu] as *mut ThreadCtx,
                        &raw const next.ctx as *const ThreadCtx,
                    );
                    // Back from the new thread — release any pending unlock
                    release_pending_unlock(cpu);
                    let _q = CPU_QUEUE_LOCKS[cpu].lock();
                    CPU_QUEUES[cpu].current = None;
                    CPU_QUEUES[cpu].idle = true;
                }
            }
        }
    }
}

pub fn init(num_cpus: usize) {
    NUM_CPUS.store(num_cpus, Ordering::Release);

    // Capture default FPU/SSE state for new threads BEFORE any thread is
    // spawned — otherwise `fxsave_template()` returns zeroed memory and the
    // first FXRSTOR loads FCW=0 (all FP exceptions unmasked).
    init_fxsave_template();

    // Initialize per-CPU idle contexts
    for cpu in 0..num_cpus {
        unsafe {
            let stack_top = IDLE_STACKS[cpu].as_mut_ptr().add(4096) as u64;
            // Push cpu_idle_loop as the return address
            let rsp = stack_top - 8;
            *(rsp as *mut u64) = cpu_idle_loop as u64;
            IDLE_CTX[cpu].rsp = rsp;
        }
    }

    // Register the main thread (tid 0) on CPU 0
    let _lock = THREAD_LOCK.lock();
    unsafe {
        extern "C" { static syscall_stack_0_top: u8; }
        THREADS[0] = Some(Thread {
            tid: 0,
            state: State::Running,
            ctx: ThreadCtx {
                rsp: 0, rbx: 0, rbp: 0, r12: 0, r13: 0, r14: 0, r15: 0, fs_base: 0,
                fxsave_area: fxsave_template(),
            },
            kernel_stack_top: &syscall_stack_0_top as *const u8 as u64,
            user_stack: 0,
            fn_ptr: 0,
            tls: 0,
            child_tid_ptr: 0,
            futex_addr: 0,
            futex_val: 0,
            in_idle_ctx: false,
            clone_r9: 0,
            clone_rip: 0,
            home_cpu: 0,
            wait_deadline_ns: 0,
            wait_timed_out: false,
            blocked_since_ns: 0,
        });
        CPU_QUEUES[0].current = Some(0);
        CPU_QUEUES[0].idle = false;
    }
}

/// Create a new thread (called from sys_clone).
/// `clone_rip` and `clone_r9` are the parent's RCX and R9 at syscall entry.
/// Create a new thread. Writes `parent_tid` and `child_tid` user-memory
/// pointers (per the `clone(2)` CLONE_PARENT_SETTID / CLONE_CHILD_SETTID
/// flags in `clone_flags`) BEFORE queueing the thread, so the child
/// can never run on another CPU and observe a TID slot that hasn't
/// been written yet.
pub fn spawn(
    fn_ptr: u64,
    stack: u64,
    tls: u64,
    child_tid: u64,
    parent_tid: u64,
    clone_flags: u64,
) -> u32 {
    // Read parent's RCX (return RIP) and R9 (fn for musl __clone child path)
    // from per-CPU GS data — safe even when both CPUs are in syscall handlers.
    let (clone_r9, clone_rip) = crate::syscall::get_clone_regs();
    let tid = NEXT_TID.fetch_add(1, Ordering::Relaxed);
    let idx = tid as usize;
    if idx >= MAX_THREADS {
        serial_println!("[sched] too many threads");
        return 0;
    }

    // Allocate kernel stack. BUG-1 Path A: reserve PREEMPT_REGION_SIZE ABOVE each
    // stack so the timer trampoline's per-thread preempt region [kstack_top .. +SIZE]
    // doesn't overlap the next thread's stack base. The usable stack stays 16 KiB.
    // (thread 0 uses syscall_stack_0 whose region is free above it — the dead
    // syscall_stack_1 — so only this allocator needs the bump. See
    // docs/STACK_ALLOCATOR_INVENTORY.md.)
    //
    // Isolation Stage 0: put a 4 KiB GUARD page BELOW each stack (the overflow
    // direction — stacks grow down) so an overflow #PFs on the guard instead of
    // silently corrupting the neighbor (ARCHITECTURE.md's top hardening item).
    // Per-slot layout, all 4 KiB-aligned so the guard stays page-aligned:
    //   [ guard 4 KiB ][ usable 16 KiB ][ pad 4 KiB ] = 24 KiB stride.
    // The 256 B preempt region lives in the top pad (invariant preserved). The
    // arena (paging::KSTACK_ARENA_*, 16 MiB) is pre-split to 4 KiB at
    // paging::init, so map_guard_page is a pure PTE clear (no TLB shootdown).
    const KSTACK_USABLE: u64 = 16384;
    const GUARD_SIZE: u64 = 4096;
    const KSTACK_STRIDE: u64 = GUARD_SIZE + KSTACK_USABLE + 4096; // 24 KiB, 4 KiB-aligned
    static KSTACK_NEXT: core::sync::atomic::AtomicU64 =
        core::sync::atomic::AtomicU64::new(crate::memory::paging::KSTACK_ARENA_BASE);
    let slot = KSTACK_NEXT.fetch_add(KSTACK_STRIDE, Ordering::Relaxed);
    // Stay within the pre-split arena (else the guard would need a runtime 2 MiB
    // split + cross-core shootdown). 16 MiB ≈ 680 stacks — far above BEAM's count.
    debug_assert!(
        slot + KSTACK_STRIDE
            <= crate::memory::paging::KSTACK_ARENA_BASE + crate::memory::paging::KSTACK_ARENA_SIZE,
        "kernel-stack arena exhausted"
    );
    let guard_page = slot;
    let kstack_base = slot + GUARD_SIZE;
    let kstack_top = kstack_base + KSTACK_USABLE;
    // SAFETY: guard_page is the dead 4 KiB below a fresh stack, inside the
    // pre-split arena; nothing has mapped-use of it. Not-present → overflow faults.
    unsafe { crate::memory::paging::map_guard_page(guard_page); }

    // Build a kernel stack frame for the child that mirrors the syscall
    // exit path. When context-switched to, the child "returns" from the
    // syscall with RAX=0 (clone returns 0 to child in Linux).
    //
    // The child's kernel stack needs the same layout as the parent's
    // stack at the point of context_switch: callee-saved regs that
    // context_switch will pop, then the ret address = clone_child_return.
    let mut ksp = kstack_top;
    unsafe {
        // Push a return address for context_switch's `ret`
        ksp -= 8;
        *(ksp as *mut u64) = clone_child_return as u64;
    }

    let _lock = THREAD_LOCK.lock();
    unsafe {
        THREADS[idx] = Some(Thread {
            tid,
            state: State::Ready,
            ctx: ThreadCtx {
                rsp: ksp,     // kernel stack with return address
                rbx: 0,       // callee-saved (restored by context_switch)
                rbp: 0,
                r12: stack,   // child's user stack
                r13: tls,     // child's TLS
                r14: child_tid,
                r15: 0,
                fs_base: tls, // initial FS_BASE — musl/ERTS may overwrite via ARCH_SET_FS
                fxsave_area: fxsave_template(),
            },
            kernel_stack_top: kstack_top,
            user_stack: stack,
            fn_ptr,
            tls,
            child_tid_ptr: child_tid,
            futex_addr: 0,
            futex_val: 0,
            in_idle_ctx: false,
            clone_r9,
            clone_rip,
            home_cpu: 0, // updated below to best_cpu
            wait_deadline_ns: 0,
            wait_timed_out: false,
            blocked_since_ns: 0,
        });
    }

    // Add to a CPU — prefer idle CPUs, then shortest queue
    let ncpus = NUM_CPUS.load(Ordering::Relaxed);
    let mut best_cpu = 0u32;
    let mut best_len = usize::MAX;
    let mut found_idle = false;
    for cpu in 0..ncpus {
        let _qlock = CPU_QUEUE_LOCKS[cpu].lock();
        let is_idle = unsafe { CPU_QUEUES[cpu].idle };
        let len = unsafe { CPU_QUEUES[cpu].queue.len() };
        // Prefer idle CPUs (they have no work)
        if is_idle && !found_idle {
            best_cpu = cpu as u32;
            best_len = len;
            found_idle = true;
        } else if !found_idle && len < best_len {
            best_len = len;
            best_cpu = cpu as u32;
        }
    }

    // Record home CPU BEFORE making the thread runnable so a
    // cross-CPU futex_wake routes correctly from the first scheduling
    // tick. (Was previously set after the push_back, leaving a brief
    // window where home_cpu=0.)
    unsafe {
        if let Some(t) = THREADS[tid as usize].as_mut() {
            t.home_cpu = best_cpu;
        }
    }

    // CLONE_PARENT_SETTID / CLONE_CHILD_SETTID: write the new TID to
    // the user pointers BEFORE we make the child runnable. Without
    // this ordering the child could run on another CPU (via the IPI
    // below) and observe the slot still holding its pre-clone value.
    const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
    const CLONE_CHILD_SETTID: u64  = 0x0100_0000;
    if (clone_flags & CLONE_PARENT_SETTID) != 0 && parent_tid != 0 {
        // 3b.2: parent_tid is a user pointer — guarded write (best-effort; a bad ptr
        // just skips the tid store, the thread is still created).
        let _ = unsafe { crate::uaccess::write_user_u32(parent_tid, tid) };
    }
    if (clone_flags & CLONE_CHILD_SETTID) != 0 && child_tid != 0 {
        // 3b.2: child_tid is a user pointer — guarded write (best-effort).
        let _ = unsafe { crate::uaccess::write_user_u32(child_tid, tid) };
    }

    {
        let _qlock = CPU_QUEUE_LOCKS[best_cpu as usize].lock();
        unsafe { CPU_QUEUES[best_cpu as usize].queue.push_back(tid); }
    }

    // If the target CPU is idle, send IPI to wake it
    let is_idle = unsafe { CPU_QUEUES[best_cpu as usize].idle };
    let cur = current_cpu();
    crate::serial_println!("[sched] cpu={} idle={} cur={}", best_cpu, is_idle, cur);
    if is_idle && best_cpu != cur {
        crate::serial_println!("[sched] sending IPI to CPU {}", best_cpu);
        crate::apic::send_ipi(best_cpu as u8);
    }

    serial_println!("[sched] thread {} created on CPU {}", tid, best_cpu);
    tid
}

/// Yield the current CPU to the next runnable thread.
pub fn yield_current() {
    // Drain any rescue requests before we take the per-CPU queue
    // lock. Safe from syscall context — we hold no locks here.
    process_rescues();

    let cpu = current_cpu() as usize;

    let switch_info: Option<(usize, usize, u64)>;
    {
        let _qlock = CPU_QUEUE_LOCKS[cpu].lock();
        // No debug printing here — serial lock contention with AP causes boot hang
        unsafe {
            let cur_tid = match CPU_QUEUES[cpu].current {
                Some(t) => t,
                None => {
                    // No current thread — this CPU is idle.
                    // If there's a thread in the queue, start running it directly.
                    if let Some(next_tid) = CPU_QUEUES[cpu].queue.pop_front() {
                        CPU_QUEUES[cpu].current = Some(next_tid);
                        CPU_QUEUES[cpu].idle = false;

                        let next_idx = next_tid as usize;
                        if let Some(next) = THREADS[next_idx].as_ref() {
                            crate::syscall::set_current_kernel_stack(next.kernel_stack_top);

                            // Jump directly to the thread's saved context.
                            // One-way switch — the idle loop doesn't need
                            // saving — but we MUST restore FS_BASE and the
                            // FPU/SSE state alongside GPRs, otherwise this
                            // thread reads TLS / XMM register state from
                            // whatever the last user of this CPU left behind.
                            // (Same invariant as context_switch.)
                            drop(_qlock);
                            let fs = next.ctx.fs_base;
                            let fxptr = &next.ctx.fxsave_area as *const _ as u64;
                            // Restore FS_BASE (clobbers rax/rdx/rcx).
                            core::arch::asm!(
                                "mov rdx, rax",
                                "shr rdx, 32",
                                "mov ecx, 0xC0000100",
                                "wrmsr",
                                in("rax") fs,
                                out("rdx") _,
                                out("rcx") _,
                            );
                            // Restore FPU/SSE state.
                            core::arch::asm!(
                                "fxrstor64 [{}]",
                                in(reg) fxptr,
                                options(nostack, preserves_flags),
                            );
                            core::arch::asm!(
                                "mov rsp, {rsp}",
                                "mov rbx, {rbx}",
                                "mov rbp, {rbp}",
                                "mov r12, {r12}",
                                "mov r13, {r13}",
                                "mov r14, {r14}",
                                "mov r15, {r15}",
                                "ret",
                                rsp = in(reg) next.ctx.rsp,
                                rbx = in(reg) next.ctx.rbx,
                                rbp = in(reg) next.ctx.rbp,
                                r12 = in(reg) next.ctx.r12,
                                r13 = in(reg) next.ctx.r13,
                                r14 = in(reg) next.ctx.r14,
                                r15 = in(reg) next.ctx.r15,
                                options(noreturn),
                            );
                        }
                    }
                    return;
                }
            };

            let next_tid = match CPU_QUEUES[cpu].queue.pop_front() {
                Some(t) => t,
                None => return,
            };

            CPU_QUEUES[cpu].queue.push_back(cur_tid);
            CPU_QUEUES[cpu].current = Some(next_tid);

            let next_kstack = THREADS[next_tid as usize].as_ref()
                .map(|t| t.kernel_stack_top).unwrap_or(0);

            switch_info = Some((cur_tid as usize, next_tid as usize, next_kstack));
        }
    } // _qlock dropped here — before context_switch

    if let Some((cur_idx, next_idx, next_kstack)) = switch_info {
        static YIELD_LOG: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        let c = YIELD_LOG.fetch_add(1, Ordering::Relaxed);
        if c < 10 {
            crate::serial::raw_str(b"[yield] switching\n");
        }
        unsafe {
            crate::syscall::set_current_kernel_stack(next_kstack);

            if let (Some(cur), Some(next)) = (THREADS[cur_idx].as_mut(), THREADS[next_idx].as_ref()) {
                context_switch(
                    &raw mut cur.ctx as *mut ThreadCtx,
                    &raw const next.ctx as *const ThreadCtx,
                );
                // After context_switch returns to us, release any pending
                // futex unlock from the thread that switched TO us.
                release_pending_unlock(current_cpu() as usize);
            }
        }
    }
}

/// Futex WAIT (no timeout) — kept for callers that don't pass a deadline.
pub fn futex_wait(addr: u64, val: u32) -> i64 {
    futex_wait_until(addr, val, None)
}

/// Futex WAIT with optional absolute deadline (in monotonic_ns units).
/// Returns 0 (woken normally), -EAGAIN (value changed), or -ETIMEDOUT (110)
/// if the deadline expires without a wake.
///
/// **Lock-handoff protocol:** the bucket lock is acquired before the value
/// check, and held continuously until AFTER context_switch completes. The
/// next thread to run on this CPU releases the lock via release_pending_unlock.
/// This closes the wake-loss race window between marking-blocked and sleeping.
///
/// **Timeout behaviour:** if `deadline` is set and we'd otherwise block
/// indefinitely, we attach the deadline to the thread (`futex_deadline_ns`)
/// and let the watchdog rescue it when the deadline passes. The watchdog's
/// rescue path now queues the woken thread, so `ethr_event_twait` returns
/// ETIMEDOUT to ERTS and the scheduler advances its timer wheel.
pub fn futex_wait_until(addr: u64, val: u32, deadline: Option<u64>) -> i64 {
    // If only 1 thread exists, yield and return (spurious wakeup).
    // This handles pre-clone musl locks that would otherwise deadlock.
    if NEXT_TID.load(Ordering::Relaxed) <= 1 {
        x86_64::instructions::interrupts::enable();
        yield_current();
        return 0;
    }

    let bucket = futex_bucket(addr);
    {
        static WAIT_LOG: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        let wc = WAIT_LOG.fetch_add(1, Ordering::Relaxed);
        if wc < 50 {
            // 3b.2: `addr` is a user futex word — guarded read (best-effort for the log).
            let cur = unsafe { crate::uaccess::read_user_u32(addr) }.unwrap_or(0);
            let cpu = current_cpu() as usize;
            let tid = unsafe { CPU_QUEUES[cpu].current.unwrap_or(0) };
            crate::serial_println!("[wait] tid={} addr={:#x} expect={:#x} cur={:#x}",
                tid, addr, val, cur);
        }
    }

    // Acquire the bucket lock. We will NOT explicitly drop it — the next
    // thread to run on this CPU will release it via release_pending_unlock.
    let flock_guard = FUTEX_LOCKS[bucket].lock();

    // Consume any pending wake for this address. This handles wake-before-wait:
    // ERTS's TSE protocol can fire a wake before the waiter has set TSE_SLEEPING
    // in the SSI flags, so the wake is "lost" by ssi_flags_set_wake clearing
    // flags before erts_tse_set is reached. Pending wakes recover this case.
    if unsafe { pending_wake_consume(bucket, addr) } {
        drop(flock_guard);
        return 0;
    }

    // SeqCst load (rather than a plain `*addr` deref) so the compiler is
    // forbidden from caching or reordering this read across the bucket-lock
    // acquire above. On x86 TSO this compiles to a regular `mov` either
    // way, but the explicit atomic operation makes the cross-CPU read
    // intent unambiguous and matches how ERTS writes the address
    // (`atomic_xchg`/store with seq_cst on the waker side).
    // 3b.2: `addr` is a user futex word — guarded read (a bad addr → EFAULT).
    let current = match unsafe { crate::uaccess::read_user_u32(addr) } {
        Ok(v) => v,
        Err(_) => {
            drop(flock_guard);
            return -14; // -EFAULT
        }
    };
    if current != val {
        drop(flock_guard);
        return -11; // -EAGAIN
    }

    // During ERTS init, yield and return (spin-yield) to avoid
    // the thread-progress registration deadlock. After init, block properly.
    if !FUTEX_BLOCKING.load(core::sync::atomic::Ordering::Acquire) {
        // Stamp the first spin-yield so the watchdog can arm blocking after a
        // generous bound if the `serial_shell ready` marker never prints.
        FIRST_SPINYIELD_NS
            .compare_exchange(0, crate::syscall::monotonic_ns(),
                              Ordering::Relaxed, Ordering::Relaxed)
            .ok();
        drop(flock_guard);
        yield_current();
        return 0;
    }

    let cpu = current_cpu() as usize;
    let blocked_tid: usize;
    unsafe {
        let cur_tid = match CPU_QUEUES[cpu].current {
            Some(t) => t,
            None => {
                drop(flock_guard);
                return 0;
            }
        };
        blocked_tid = cur_tid as usize;
        // Mark thread as blocked (under bucket lock — prevents wake race)
        if let Some(thread) = THREADS[blocked_tid].as_mut() {
            thread.state = State::Blocked;
            thread.futex_addr = addr;
            thread.futex_val = val;
            thread.wait_deadline_ns = deadline.unwrap_or(0);
            thread.wait_timed_out = false;
            thread.blocked_since_ns = crate::syscall::monotonic_ns();
        }
    }

    // Pick the next thread to run (or go idle).
    // We DO release the queue lock before context_switch — only the bucket
    // lock crosses the switch boundary.
    let switch_info: Option<(usize, u64)>;
    {
        let _qlock = CPU_QUEUE_LOCKS[cpu].lock();
        unsafe {
            let next_tid = CPU_QUEUES[cpu].queue.pop_front();
            match next_tid {
                Some(next) => {
                    CPU_QUEUES[cpu].current = Some(next);
                    let kstack = THREADS[next as usize].as_ref()
                        .map(|t| t.kernel_stack_top).unwrap_or(0);
                    switch_info = Some((next as usize, kstack));
                }
                None => {
                    // No other thread — go idle, wait for IPI
                    CPU_QUEUES[cpu].current = None;
                    CPU_QUEUES[cpu].idle = true;
                    switch_info = None;
                }
            }
        }
    } // queue lock dropped

    // Hand the bucket lock off to the next thread that runs on this CPU.
    // We `forget` the guard so its Drop doesn't run; release_pending_unlock
    // does the actual release after context_switch.
    PENDING_UNLOCK_BUCKET[cpu].store(bucket as i32, Ordering::Release);
    core::mem::forget(flock_guard);

    match switch_info {
        Some((next_idx, kstack)) => {
            unsafe {
                crate::syscall::set_current_kernel_stack(kstack);
                if let (Some(cur), Some(nxt)) = (THREADS[blocked_tid].as_mut(), THREADS[next_idx].as_ref()) {
                    context_switch(
                        &raw mut cur.ctx as *mut ThreadCtx,
                        &raw const nxt.ctx as *const ThreadCtx,
                    );
                }
            }
            // We were resumed after a wake. Release any pending unlock from
            // the thread that switched TO us before it switched away.
            release_pending_unlock(current_cpu() as usize);
            // If we were woken because of a deadline timeout (watchdog set
            // wait_timed_out), report ETIMEDOUT so ERTS's ethr_event_twait
            // returns to the scheduler and lets it advance the timer wheel.
            unsafe {
                if let Some(t) = THREADS[blocked_tid].as_mut() {
                    let to = t.wait_timed_out;
                    t.wait_timed_out = false;
                    t.wait_deadline_ns = 0;
                    if to { return -110; } // -ETIMEDOUT
                }
            }
            0 // woken
        }
        None => {
            // No other thread on this CPU. Context-switch to the per-CPU
            // idle loop. The idle loop only checks the run queue, so no
            // side-channel tracking is needed — when futex_wake runs, it
            // pushes this thread to the queue and the idle loop picks it up.
            let cpu = current_cpu() as usize;
            unsafe {
                if let Some(thread) = THREADS[blocked_tid].as_mut() {
                    context_switch(
                        &raw mut thread.ctx as *mut ThreadCtx,
                        &raw const IDLE_CTX[cpu] as *const ThreadCtx,
                    );
                }
                // We were woken and context_switched back. Release any
                // pending unlock from the cpu_idle_loop side, then return 0.
                release_pending_unlock(current_cpu() as usize);
                if let Some(t) = THREADS[blocked_tid].as_mut() {
                    let to = t.wait_timed_out;
                    t.wait_timed_out = false;
                    t.wait_deadline_ns = 0;
                    if to { return -110; }
                }
            }
            0
        }
    }
}

/// Futex WAKE — wake up to `count` threads sleeping on addr.
pub fn futex_wake(addr: u64, count: u32) -> i64 {
    let bucket = futex_bucket(addr);
    let _flock = FUTEX_LOCKS[bucket].lock();
    {
        static WAKE_CALL_LOG: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        let wc = WAKE_CALL_LOG.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if wc < 50 {
            // 3b.2: `addr` is a user futex word — guarded read (best-effort for the log).
            let val = unsafe { crate::uaccess::read_user_u32(addr) }.unwrap_or(0);
            crate::serial_println!("[wake_call] addr={:#x} count={} val={:#x}", addr, count, val);
        }
    }

    let mut woken = 0i64;
    let _tlock = THREAD_LOCK.lock();

    unsafe {
        for i in 0..MAX_THREADS {
            if woken >= count as i64 { break; }
            if let Some(thread) = THREADS[i].as_mut() {
                if thread.state == State::Blocked && thread.futex_addr == addr {
                    thread.state = State::Ready;
                    thread.futex_addr = 0;
                    thread.blocked_since_ns = 0;
                    woken += 1;
                    static WAKE_LOG: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
                    let wc = WAKE_LOG.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    if wc < 30 {
                        crate::serial_println!("[wake] tid={} addr={:#x}", thread.tid, addr);
                    }

                    // Unified queue: ALWAYS push the woken thread to its
                    // home CPU's run queue. The idle loop's only source of
                    // truth is the queue — no side-channel tracking.
                    let target_cpu = thread.home_cpu;
                    thread.in_idle_ctx = false;
                    {
                        let _qlock = CPU_QUEUE_LOCKS[target_cpu as usize].lock();
                        CPU_QUEUES[target_cpu as usize].queue.push_back(thread.tid);
                    }
                    // Always IPI the target CPU if it's not us — ensures the CPU
                    // wakes from hlt and picks up the queued thread promptly.
                    if target_cpu != current_cpu() {
                        crate::apic::send_ipi(target_cpu as u8);
                    }
                }
            }
        }
        // Wake-before-wait: if no waiter was found, leave a pending wake at
        // this address. The next futex_wait at the same address will consume
        // it and return immediately. Required for ERTS's TSE protocol where
        // erts_tse_set can fire before the waiter completes erts_tse_wait setup.
        if woken == 0 {
            pending_wake_insert(bucket, addr);
            static PW_LOG: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
            let wc = PW_LOG.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if wc < 20 {
                crate::serial_println!("[pending_wake] addr={:#x}", addr);
            }
        }
    }

    woken
}

/// Per-CPU "reschedule needed" flag. Set by the timer, checked at syscall exit.
static NEED_RESCHED: [AtomicBool; MAX_CPUS] = {
    const F: AtomicBool = AtomicBool::new(false);
    [F; MAX_CPUS]
};

/// Per-thread "rescue requested" flag. Set by the watchdog (which runs
/// in timer-interrupt context and therefore cannot safely acquire the
/// futex / thread / queue locks). Drained by `process_rescues()` at
/// safe scheduler points (idle loop, yield_current, check_resched).
///
/// Previously the watchdog mutated thread state and pushed to the run
/// queue directly from interrupt context. That races against
/// `futex_wait`'s lock-handoff protocol (bucket lock spans the
/// wait→sleep transition) and `futex_wake`'s state mutation: the
/// watchdog could double-queue a thread, observe a Blocked thread
/// mid-context_switch, or interrupt a pending unlock handoff and
/// leave the bucket lock orphaned.
static RESCUE_REQUESTED: [AtomicBool; MAX_THREADS] = {
    const F: AtomicBool = AtomicBool::new(false);
    [F; MAX_THREADS]
};

/// Periodically wake blocked threads (watchdog). Called every ~1 second.
/// This handles missed futex_wake events where a thread wrote 0 to a lock
/// before the waiter called futex_wait, causing a permanent block.
pub fn watchdog_wake() {
    // Runs in timer-interrupt context — no locks held, no mutation
    // allowed on shared state. Only sets `RESCUE_REQUESTED[i]` for
    // threads whose condition would warrant a rescue; the actual
    // state transition is performed by `process_rescues()` from a
    // safe scheduler point (idle loop / yield / syscall exit) where
    // it can take the futex / thread / queue locks in the same order
    // as `futex_wake`.
    let now_ns = crate::syscall::monotonic_ns();

    // Belt-and-braces valve backstop: the primary arm is the `serial_shell
    // ready` marker (syscall.rs). If that never prints (boot reordered, app
    // failed before apply_config, marker text changed), arm blocking anyway a
    // generous bound after the first spin-yield so idle CPUs reach HLT instead
    // of spinning forever. Only fires while still in spin-yield mode.
    if !FUTEX_BLOCKING.load(Ordering::Acquire) {
        let t0 = FIRST_SPINYIELD_NS.load(Ordering::Relaxed);
        if t0 != 0 && now_ns >= t0 + BLOCKING_ARM_FALLBACK_NS {
            FUTEX_BLOCKING.store(true, Ordering::Release);
            crate::serial::set_quiet(false);
            crate::serial_println!(
                "[sched] FALLBACK: arming blocking futex {}s after first spin-yield \
                 (serial_shell ready marker never seen — check tyn_boot boot path)",
                BLOCKING_ARM_FALLBACK_NS / 1_000_000_000
            );
        }
    }

    unsafe {
        for i in 0..MAX_THREADS {
            // Snapshot read — interrupt context, no locks.
            let Some(thread) = THREADS[i].as_ref() else { continue };
            if thread.state != State::Blocked { continue; }
            if thread.futex_addr == 0 { continue; }

            // Did the futex value change since the waiter recorded its
            // expected value? Lost-wake backstop. The lock-handoff
            // protocol in futex_wait spans the wait→sleep transition
            // and shouldn't lose wakes, but this catches the
            // value-changed-without-wake bug class as a safety net.
            // 3b.2: futex_addr is a user word — guarded read (skip the thread on EFAULT).
            let Ok(current) = crate::uaccess::read_user_u32(thread.futex_addr) else { continue };
            let value_changed = current != thread.futex_val;

            // Timed wait expired (used by ethr_event_twait to drive
            // ERTS's timer wheel — `receive after N` / gen_server:call
            // timeouts depend on this).
            let timed_out = thread.wait_deadline_ns != 0
                && now_ns >= thread.wait_deadline_ns;

            // Stall safety net: an infinite-wait thread that has been
            // Blocked for > BLOCKED_RESCUE_NS gets a spurious-wake-style
            // rescue. ERTS's TSE event loop tolerates spurious wakes
            // (it re-checks and re-waits), so this is a no-op in the
            // healthy case and a rescue in the stuck case.
            const BLOCKED_RESCUE_NS: u64 = 5_000_000_000; // 5 s
            let stale = thread.wait_deadline_ns == 0
                && thread.blocked_since_ns != 0
                && now_ns >= thread.blocked_since_ns + BLOCKED_RESCUE_NS;

            if value_changed || timed_out || stale {
                RESCUE_REQUESTED[i].store(true, Ordering::Release);
            }
        }
    }
}

/// Drain pending rescue requests from `RESCUE_REQUESTED`. Acquires the
/// same lock set as `futex_wake` (bucket → THREAD_LOCK → queue) so the
/// state transition is protocol-safe. Called from non-interrupt
/// scheduler points only.
fn process_rescues() {
    for i in 0..MAX_THREADS {
        if !RESCUE_REQUESTED[i].swap(false, Ordering::Acquire) { continue; }

        unsafe {
            // Read the futex address WITHOUT holding any locks first so
            // we know which bucket lock to take.
            let Some(thread) = THREADS[i].as_ref() else { continue };
            if thread.state != State::Blocked { continue; }
            let addr = thread.futex_addr;
            if addr == 0 { continue; }
            let bucket = futex_bucket(addr);

            // Now take the locks in the same order as futex_wake.
            let _flock = FUTEX_LOCKS[bucket].lock();
            let _tlock = THREAD_LOCK.lock();

            // Re-check under lock — the thread may have been woken
            // through the normal futex_wake path between the watchdog
            // setting the flag and us getting here.
            let Some(thread) = THREADS[i].as_mut() else { continue };
            if thread.state != State::Blocked { continue; }

            // Compute deadline-expiry under lock so wait_timed_out
            // reflects the actual state at rescue time.
            let now_ns = crate::syscall::monotonic_ns();
            let was_timed_out = thread.wait_deadline_ns != 0
                && now_ns >= thread.wait_deadline_ns;

            thread.state = State::Ready;
            thread.futex_addr = 0;
            thread.blocked_since_ns = 0;
            if was_timed_out {
                thread.wait_timed_out = true;
            }
            thread.wait_deadline_ns = 0;

            let tid = thread.tid;
            let target_cpu = thread.home_cpu as usize;

            if target_cpu < MAX_CPUS {
                let _qlock = CPU_QUEUE_LOCKS[target_cpu].lock();
                if !CPU_QUEUES[target_cpu].queue.iter().any(|&t| t == tid) {
                    CPU_QUEUES[target_cpu].queue.push_back(tid);
                }
            }
            if target_cpu != current_cpu() as usize {
                crate::apic::send_ipi(target_cpu as u8);
            }

            static WD_LOG: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
            let c = WD_LOG.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if c < 5 {
                crate::serial_println!("[rescue] tid={} addr={:#x}", tid, addr);
            }
        }
    }
}

/// Called from timer interrupt on each CPU.
/// Does NOT context-switch — just sets a flag. The actual switch happens
/// at syscall exit (check_resched). This avoids IST stack corruption
/// when multiple threads on the same CPU get timer-preempted.
pub fn timer_tick() {
    let cpu = current_cpu() as usize;
    if cpu < MAX_CPUS {
        NEED_RESCHED[cpu].store(true, Ordering::Release);
    }
}

/// Check if a reschedule is needed and yield if so. Called from syscall exit.
pub fn check_resched() {
    // Drain rescue requests on every syscall exit, even if no resched
    // is otherwise needed. Syscall exit is the most-frequent safe
    // point — keeps rescues responsive without piling up.
    process_rescues();
    let cpu = current_cpu() as usize;
    if cpu < MAX_CPUS && NEED_RESCHED[cpu].swap(false, Ordering::Acquire) {
        yield_current();
    }
}

/// Exit the current thread. Marks it as dead and switches away (never returns).
pub fn thread_exit() {
    let cpu = current_cpu() as usize;
    unsafe {
        let cur_tid = {
            let _qlock = CPU_QUEUE_LOCKS[cpu].lock();
            let tid = CPU_QUEUES[cpu].current.take(); // remove from current
            CPU_QUEUES[cpu].idle = true;
            tid
        };
        if let Some(tid) = cur_tid {
            if let Some(thread) = THREADS[tid as usize].as_mut() {
                thread.state = State::Dead;
            }
        }
        // Switch to idle context (never returns from this thread's perspective)
        context_switch(
            // Use a throwaway context (the dead thread's ctx, which we'll never restore)
            &raw mut THREADS[cur_tid.unwrap_or(0) as usize].as_mut().unwrap().ctx as *mut ThreadCtx,
            &raw const IDLE_CTX[cpu] as *const ThreadCtx,
        );
    }
    loop { x86_64::instructions::hlt(); }
}

/// Child return from clone: set TLS, switch to user stack, return 0 via
/// the syscall exit path. This makes the child return from clone(2) with 0,
/// which is what musl's __clone expects. musl then runs pthread_create's
/// cleanup code (releasing __thread_list_lock) before calling the thread fn.
extern "C" fn clone_child_return() {
    // We were just context_switched to (the parent's futex_wait may have
    // handed off a bucket lock). Release it before doing anything else.
    release_pending_unlock(current_cpu() as usize);

    // r12 = child user stack, r13 = TLS (set by context_switch restore)
    let stack: u64;
    let tls: u64;
    unsafe {
        core::arch::asm!("mov {}, r12", out(reg) stack);
        core::arch::asm!("mov {}, r13", out(reg) tls);
    }

    // Set child's TLS (FS_BASE)
    if tls != 0 {
        unsafe {
            x86_64::registers::model_specific::Msr::new(0xC000_0100).write(tls);
        }
    }

    // Read saved R9 (fn pointer) and RIP (return address) from this thread
    let r9: u64;
    let rcx: u64;
    let cur = current_cpu() as usize;
    let cur_tid = unsafe { CPU_QUEUES[cur].current.unwrap_or(0) as usize };
    unsafe {
        if let Some(thread) = THREADS[cur_tid].as_ref() {
            r9 = thread.clone_r9;
            rcx = thread.clone_rip;
        } else {
            r9 = 0;
            rcx = 0;
        }
    }

    // Verify FS_BASE was set on this CPU
    let fs_check = unsafe { x86_64::registers::model_specific::Msr::new(0xC000_0100).read() };
    crate::serial_println!("[child] stack={:#x} rip={:#x} r9={:#x} fs={:#x} cpu={}",
        stack, rcx, r9, fs_check, current_cpu());

    // Switch to child's user stack and return to musl's __clone child path.
    unsafe {
        // Stage 3a: a cloned BEAM scheduler enters RING 3 too (iretq to DPL=3),
        // not a ring-0 jmp — else it would run at a different privilege than the
        // main thread. Same shape as jump_to_user: cli, swapgs, build the iretq
        // frame with the child's user stack + resume RIP, set r9=fn / rax=0, iretq.
        let (ucode, udata) = crate::percpu::user_selectors();
        let cs = (ucode | 3) as u64;
        let ss = (udata | 3) as u64;
        core::arch::asm!(
            "cli",
            "swapgs",
            "push {ss}",
            "push {stack}",
            "push 0x202",       // RFLAGS: IF=1 + reserved bit 1
            "push {cs}",
            "push {rcx}",       // resume RIP (child path after __clone)
            "mov r9, {r9}",     // child fn pointer (musl __clone)
            "xor eax, eax",     // RAX = 0 (clone returns 0 to the child)
            "iretq",
            stack = in(reg) stack,
            r9 = in(reg) r9,
            rcx = in(reg) rcx,
            cs = in(reg) cs,
            ss = in(reg) ss,
            options(noreturn),
        );
    }
}

/// Low-level context switch.
///
/// Saves and restores: callee-saved GPRs (rsp, rbx, rbp, r12-r15), FS_BASE
/// (TLS pointer, MSR 0xC000_0100), and the FPU/SSE state (FXSAVE/FXRSTOR
/// area at offset 64 in ThreadCtx — 512 bytes, must be 16-byte aligned).
///
/// Without FXSAVE/FXRSTOR, ERTS and musl SSE-using code (memcpy/memset via
/// movdqa, FP arithmetic) would see XMM register contents from whichever
/// thread last ran on this CPU. That manifests as random data corruption —
/// different beam_load failures and pointer-deref page faults each run.
#[unsafe(naked)]
extern "C" fn context_switch(_from: *mut ThreadCtx, _to: *const ThreadCtx) {
    core::arch::naked_asm!(
        // Save callee-saved GPRs of outgoing thread.
        "mov [rdi], rsp",
        "mov [rdi+8], rbx",
        "mov [rdi+16], rbp",
        "mov [rdi+24], r12",
        "mov [rdi+32], r13",
        "mov [rdi+40], r14",
        "mov [rdi+48], r15",
        // Save outgoing FPU/SSE state.
        "fxsave64 [rdi+64]",
        // Save outgoing FS_BASE: RDMSR(0xC000_0100) -> EDX:EAX
        "push rsi",                  // preserve to-ptr (rdmsr clobbers eax/ecx/edx)
        "push rdi",                  // preserve from-ptr
        "mov ecx, 0xC0000100",
        "rdmsr",
        "shl rdx, 32",
        "or rax, rdx",
        "pop rdi",
        "mov [rdi+56], rax",
        "pop rsi",
        // Restore incoming GPRs.
        "mov rsp, [rsi]",
        "mov rbx, [rsi+8]",
        "mov rbp, [rsi+16]",
        "mov r12, [rsi+24]",
        "mov r13, [rsi+32]",
        "mov r14, [rsi+40]",
        "mov r15, [rsi+48]",
        // Restore incoming FS_BASE: WRMSR(0xC000_0100) <- EDX:EAX
        "mov rax, [rsi+56]",
        "mov rdx, rax",
        "shr rdx, 32",
        "mov ecx, 0xC0000100",
        "wrmsr",
        // Restore incoming FPU/SSE state.
        "fxrstor64 [rsi+64]",
        "ret",
    );
}

// --- Compatibility shims for existing code ---

/// Current thread index (for syscall.rs compatibility)
pub fn current_idx() -> usize {
    let cpu = current_cpu() as usize;
    unsafe {
        CPU_QUEUES[cpu].current.unwrap_or(0) as usize
    }
}

pub fn has_child() -> bool {
    NEXT_TID.load(Ordering::Relaxed) > 1
}

pub fn num_threads() -> usize {
    NEXT_TID.load(Ordering::Relaxed) as usize
}
