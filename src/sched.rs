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

/// When false, futex_wait returns immediately (spin-yield mode for ERTS init).
/// Switched to true once ERTS finishes thread-progress registration.
static FUTEX_BLOCKING: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(true);

/// Enable blocking futex. Called once ERTS init is past the thread-progress barrier.
pub fn enable_blocking_futex() {
    FUTEX_BLOCKING.store(true, core::sync::atomic::Ordering::Release);
    crate::serial_println!("[sched] blocking futex enabled");
}

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

        x86_64::instructions::interrupts::enable();
        x86_64::instructions::hlt();

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
        });
        CPU_QUEUES[0].current = Some(0);
        CPU_QUEUES[0].idle = false;
    }
}

/// Create a new thread (called from sys_clone).
/// `clone_rip` and `clone_r9` are the parent's RCX and R9 at syscall entry.
pub fn spawn(fn_ptr: u64, stack: u64, tls: u64, child_tid: u64) -> u32 {
    // Read parent's RCX (return RIP) and R9 (fn for musl __clone child path)
    // from per-CPU GS data — safe even when both CPUs are in syscall handlers.
    let (clone_r9, clone_rip) = crate::syscall::get_clone_regs();
    let tid = NEXT_TID.fetch_add(1, Ordering::Relaxed);
    let idx = tid as usize;
    if idx >= MAX_THREADS {
        serial_println!("[sched] too many threads");
        return 0;
    }

    // Allocate kernel stack
    static KSTACK_NEXT: core::sync::atomic::AtomicU64 =
        core::sync::atomic::AtomicU64::new(0x0700_0000);
    let kstack_base = KSTACK_NEXT.fetch_add(16384, Ordering::Relaxed);
    let kstack_top = kstack_base + 16384;

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

    {
        let _qlock = CPU_QUEUE_LOCKS[best_cpu as usize].lock();
        unsafe { CPU_QUEUES[best_cpu as usize].queue.push_back(tid); }
    }
    // Record home CPU for futex_wake routing
    unsafe {
        if let Some(t) = THREADS[tid as usize].as_mut() {
            t.home_cpu = best_cpu;
        }
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

/// Futex WAIT — atomically check *addr == val and sleep.
/// Returns 0 (woken) or -EAGAIN (value changed).
///
/// **Lock-handoff protocol:** the bucket lock is acquired before the value
/// check, and held continuously until AFTER context_switch completes. The
/// next thread to run on this CPU releases the lock via release_pending_unlock.
/// This closes the wake-loss race window between marking-blocked and sleeping.
pub fn futex_wait(addr: u64, val: u32) -> i64 {
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
            let cur = unsafe { *(addr as *const u32) };
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

    let current = unsafe { *(addr as *const u32) };
    if current != val {
        drop(flock_guard);
        return -11; // -EAGAIN
    }

    // During ERTS init, yield and return (spin-yield) to avoid
    // the thread-progress registration deadlock. After init, block properly.
    if !FUTEX_BLOCKING.load(core::sync::atomic::Ordering::Acquire) {
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
            let val = unsafe { *(addr as *const u32) };
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

/// Periodically wake blocked threads (watchdog). Called every ~1 second.
/// This handles missed futex_wake events where a thread wrote 0 to a lock
/// before the waiter called futex_wait, causing a permanent block.
pub fn watchdog_wake() {
    static WD_CHECKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    let check_num = WD_CHECKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    // Skip the lock — the watchdog runs in timer interrupt context.
    // It only reads thread state and sets state=Ready, which is safe
    // because state is only checked via read_volatile in the idle loop.
    // Using try_lock deadlocks when futex_wake holds the lock on the same CPU.
    unsafe {
        for i in 0..MAX_THREADS {
            if let Some(thread) = THREADS[i].as_mut() {
                if thread.state == State::Blocked {
                    // Check if the futex value changed (lock was released)
                    let current = *(thread.futex_addr as *const u32);
                    if check_num < 1 {
                        crate::serial_println!("[wd] tid={} addr={:#x} val={:#x} cur={:#x}",
                            thread.tid, thread.futex_addr, thread.futex_val, current);
                    }
                    // With the lock-handoff protocol in futex_wait, real wakes
                    // are never lost — the bucket lock spans the wait→sleep
                    // transition. The watchdog is now a backstop only for
                    // genuine value-changed-without-wake bugs.
                    let force = false;
                    if current != thread.futex_val || force {
                        // Value changed or force-wake timeout.
                        // Just set state=Ready — DON'T add to queue.
                        // The idle loop on the thread's CPU detects the state
                        // change and resumes via context_switch. Adding to queue
                        // would cause dual scheduling.
                        thread.state = State::Ready;
                        let addr = thread.futex_addr;
                        thread.futex_addr = 0;
                        let tid = thread.tid;

                        static WD_LOG: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
                        let c = WD_LOG.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                        if c < 5 {
                            crate::serial_println!("[watchdog] woke tid={} addr={:#x} old_val={:#x} cur={:#x}",
                                tid, addr, thread.futex_val, current);
                        }
                    }
                }
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
        core::arch::asm!(
            "mov rsp, {stack}",
            "mov r9, {r9}",
            "xor eax, eax",  // RAX = 0
            "sti",
            "jmp {rcx}",
            stack = in(reg) stack,
            r9 = in(reg) r9,
            rcx = in(reg) rcx,
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
