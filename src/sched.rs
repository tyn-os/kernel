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
#[repr(C)]
struct ThreadCtx {
    rsp: u64,
    rbx: u64,
    rbp: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
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
    const EMPTY: ThreadCtx = ThreadCtx { rsp: 0, rbx: 0, rbp: 0, r12: 0, r13: 0, r14: 0, r15: 0 };
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

static NUM_CPUS: AtomicUsize = AtomicUsize::new(1);

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
        x86_64::instructions::interrupts::enable();
        x86_64::instructions::hlt();

        let blocked = unsafe { IDLE_BLOCKED_TID[cpu] };

        // Check if the blocked thread was woken by futex_wake.
        // Use read_volatile to prevent the compiler from hoisting this
        // read out of the loop — another CPU modifies the state field.
        let woken = unsafe {
            THREADS[blocked].as_ref()
                .map(|t| core::ptr::read_volatile(&t.state) == State::Ready)
                .unwrap_or(false)
        };
        if woken {
            unsafe {
                // Remove the thread from the queue if futex_wake also added it there.
                // Without this, the thread would be both current AND in the queue.
                {
                    let _qlock = CPU_QUEUE_LOCKS[cpu].lock();
                    CPU_QUEUES[cpu].queue.retain(|&tid| tid != blocked as u32);
                }
            }
            unsafe {
                let _qlock = CPU_QUEUE_LOCKS[cpu].lock();
                CPU_QUEUES[cpu].current = Some(blocked as u32);
                CPU_QUEUES[cpu].idle = false;

                if let Some(thread) = THREADS[blocked].as_ref() {
                    crate::syscall::set_current_kernel_stack(thread.kernel_stack_top);
                    context_switch(
                        &raw mut IDLE_CTX[cpu] as *mut ThreadCtx,
                        &raw const thread.ctx as *const ThreadCtx,
                    );
                    // Returns here when this CPU idles again
                }
            }
            continue;
        }

        // Check for new threads in the queue
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
                    // Back from the new thread — set idle again
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
            ctx: ThreadCtx { rsp: 0, rbx: 0, rbp: 0, r12: 0, r13: 0, r14: 0, r15: 0 },
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

                            // Jump directly to the thread's saved context
                            // This is a one-way switch — the idle loop doesn't need saving
                            drop(_qlock);
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
            }
        }
    }
}

/// Futex WAIT — atomically check *addr == val and sleep.
/// Returns 0 (woken) or -EAGAIN (value changed).
pub fn futex_wait(addr: u64, val: u32) -> i64 {
    // If only 1 thread exists, yield and return (spurious wakeup).
    // This handles pre-clone musl locks that would otherwise deadlock.
    if NEXT_TID.load(Ordering::Relaxed) <= 1 {
        x86_64::instructions::interrupts::enable();
        yield_current();
        return 0;
    }

    let bucket = futex_bucket(addr);

    // Atomic check under the futex lock
    {
        let _flock = FUTEX_LOCKS[bucket].lock();
        let current = unsafe { *(addr as *const u32) };
        if current != val {
            return -11; // -EAGAIN
        }

        // (futex_wait blocking log removed for clean output)

        let cpu = current_cpu() as usize;
        unsafe {
            let cur_tid = match CPU_QUEUES[cpu].current {
                Some(t) => t,
                None => return 0,
            };
            // Mark thread as blocked (under futex lock — prevents wake race)
            if let Some(thread) = THREADS[cur_tid as usize].as_mut() {
                thread.state = State::Blocked;
                thread.futex_addr = addr;
                thread.futex_val = val;
            }
        }
    } // futex lock dropped — safe for other threads to wake us now

    // Schedule next thread (locks dropped before context switch)
    let cpu = current_cpu() as usize;
    let blocked_tid: usize;
    let switch_info: Option<(usize, usize, u64)>;
    {
        let _qlock = CPU_QUEUE_LOCKS[cpu].lock();
        unsafe {
            let cur_tid = CPU_QUEUES[cpu].current.unwrap();
            blocked_tid = cur_tid as usize;
            let next_tid = CPU_QUEUES[cpu].queue.pop_front();
            match next_tid {
                Some(next) => {
                    CPU_QUEUES[cpu].current = Some(next);
                    let kstack = THREADS[next as usize].as_ref()
                        .map(|t| t.kernel_stack_top).unwrap_or(0);
                    switch_info = Some((cur_tid as usize, next as usize, kstack));
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

    match switch_info {
        Some((cur_idx, next_idx, kstack)) => {
            unsafe {
                crate::syscall::set_current_kernel_stack(kstack);
                if let (Some(cur), Some(nxt)) = (THREADS[cur_idx].as_mut(), THREADS[next_idx].as_ref()) {
                    context_switch(
                        &raw mut cur.ctx as *mut ThreadCtx,
                        &raw const nxt.ctx as *const ThreadCtx,
                    );
                }
            }
            0 // woken
        }
        None => {
            // No other thread on this CPU. Context-switch to the per-CPU
            // idle loop. This properly saves the blocked thread's register
            // state so that futex_wake can later resume it on any CPU.
            let cpu = current_cpu() as usize;
            unsafe {
                IDLE_BLOCKED_TID[cpu] = blocked_tid;
                if let Some(t) = THREADS[blocked_tid].as_mut() {
                    t.in_idle_ctx = true;
                }
                // context_switch saves our regs to the blocked thread's ctx
                // and loads the idle context. When we're woken, context_switch
                // restores our regs and we return here.
                if let Some(thread) = THREADS[blocked_tid].as_mut() {
                    context_switch(
                        &raw mut thread.ctx as *mut ThreadCtx,
                        &raw const IDLE_CTX[cpu] as *const ThreadCtx,
                    );
                }
                // We were woken and context_switched back. Return 0.
            }
            0
        }
    }
}

/// Futex WAKE — wake up to `count` threads sleeping on addr.
pub fn futex_wake(addr: u64, count: u32) -> i64 {
    let bucket = futex_bucket(addr);
    let _flock = FUTEX_LOCKS[bucket].lock();

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

                    let target_cpu = (i % NUM_CPUS.load(Ordering::Relaxed)) as u32;
                    if thread.in_idle_ctx {
                        // Thread is in per-CPU idle context — don't add to queue.
                        // The idle loop detects state=Ready and resumes via context_switch.
                        thread.in_idle_ctx = false;
                    } else {
                        // Thread was context_switched normally — add to queue.
                        let _qlock = CPU_QUEUE_LOCKS[target_cpu as usize].lock();
                        CPU_QUEUES[target_cpu as usize].queue.push_back(thread.tid);
                    }
                    if CPU_QUEUES[target_cpu as usize].idle && target_cpu != current_cpu() {
                        crate::apic::send_ipi(target_cpu as u8);
                    }
                }
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
    let _tlock = THREAD_LOCK.lock();
    unsafe {
        for i in 0..MAX_THREADS {
            if let Some(thread) = THREADS[i].as_mut() {
                if thread.state == State::Blocked {
                    // Check if the futex value changed (lock was released)
                    let current = *(thread.futex_addr as *const u32);
                    if check_num < 3 {
                        crate::serial_println!("[wd] tid={} addr={:#x} val={:#x} cur={:#x}",
                            thread.tid, thread.futex_addr, thread.futex_val, current);
                    }
                    // Force-wake threads stuck on ERTS init sync (val=0x2)
                    // after 3 seconds. ERTS handles spurious wakeups via recheck.
                    let force = check_num >= 2; // force-wake ALL stuck threads after 2 seconds
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
                        if c < 20 {
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

/// Child return from clone: set TLS, switch to user stack, return 0 via
/// the syscall exit path. This makes the child return from clone(2) with 0,
/// which is what musl's __clone expects. musl then runs pthread_create's
/// cleanup code (releasing __thread_list_lock) before calling the thread fn.
extern "C" fn clone_child_return() {
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

/// Low-level context switch (same as old thread.rs).
#[unsafe(naked)]
extern "C" fn context_switch(_from: *mut ThreadCtx, _to: *const ThreadCtx) {
    core::arch::naked_asm!(
        "mov [rdi], rsp",
        "mov [rdi+8], rbx",
        "mov [rdi+16], rbp",
        "mov [rdi+24], r12",
        "mov [rdi+32], r13",
        "mov [rdi+40], r14",
        "mov [rdi+48], r15",
        "mov rsp, [rsi]",
        "mov rbx, [rsi+8]",
        "mov rbp, [rsi+16]",
        "mov r12, [rsi+24]",
        "mov r13, [rsi+32]",
        "mov r14, [rsi+40]",
        "mov r15, [rsi+48]",
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
