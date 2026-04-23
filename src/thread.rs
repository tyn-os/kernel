//! Cooperative threading for ERTS — supports main thread + up to N children.
//! Context switches happen on yield points (futex, sched_yield, pipe read, epoll_wait).

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use crate::serial_println;

/// Saved CPU context for cooperative switching.
#[repr(C)]
struct Context {
    rsp: u64,
    rbx: u64,
    rbp: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    valid: bool,
    kernel_stack_top: u64, // per-thread kernel stack for syscall handling
    ist_stack_top: u64,    // per-thread IST stack for timer interrupts
    sleeping: bool,        // true if blocked on futex WAIT
    futex_addr: u64,       // address this thread is waiting on (if sleeping)
    futex_val: u32,        // value it was waiting for
    tsc_offset: i64,       // TSC_ADJUST value for this thread
}

impl Context {
    const fn empty() -> Self {
        Context { rsp: 0, rbx: 0, rbp: 0, r12: 0, r13: 0, r14: 0, r15: 0,
                  valid: false, kernel_stack_top: 0, ist_stack_top: 0,
                  sleeping: false, futex_addr: 0, futex_val: 0, tsc_offset: 0 }
    }
}

const MAX_THREADS: usize = 24; // main + up to 23 children (ERTS creates ~16-18)

/// Kernel stack size per thread.
const KSTACK_SIZE: u64 = 16384;
/// Bump allocator for kernel stacks (identity-mapped region below mmap).
static KSTACK_NEXT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0x0700_0000); // 112 MiB

static mut CONTEXTS: [Context; MAX_THREADS] = {
    const E: Context = Context::empty();
    [E; MAX_THREADS]
};

/// Pending wake addresses — when futex_wake finds nobody sleeping,
/// we store the address here so check_futex_waiters can force-wake
/// any thread that subsequently sleeps on it.
const MAX_PENDING: usize = 32;
static mut PENDING_WAKES: [u64; MAX_PENDING] = [0; MAX_PENDING];
static PENDING_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Index of the currently running thread (0 = main).
static CURRENT: AtomicUsize = AtomicUsize::new(0);
/// Total number of threads (1 = main only, 2+ = main + children).
static NUM_THREADS: AtomicUsize = AtomicUsize::new(1);
static YIELD_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Bump allocator for per-thread IST stacks (4 KiB each).
const IST_STACK_SIZE: u64 = 4096;
static IST_NEXT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0x0680_0000); // 104 MiB (below kernel stacks at 112 MiB)

/// Mark the main thread context as valid with its kernel stack.
pub fn init_main() {
    // SAFETY: Called once during init before any threads exist.
    unsafe {
        extern "C" { static syscall_stack_0_top: u8; }
        CONTEXTS[0].valid = true;
        CONTEXTS[0].kernel_stack_top = &syscall_stack_0_top as *const u8 as u64;
        // Allocate IST stack for main thread and update TSS
        let ist_base = IST_NEXT.fetch_add(IST_STACK_SIZE, Ordering::Relaxed);
        CONTEXTS[0].ist_stack_top = ist_base + IST_STACK_SIZE;
        crate::interrupts::set_ist1(ist_base + IST_STACK_SIZE);
    }
}

/// Register a child thread to run later. Called from sys_clone.
///
/// # Safety
/// `fn_ptr` must be a valid function pointer, `stack` must be a valid
/// child stack set up by musl's __clone (with arg at [stack]).
pub unsafe fn spawn(fn_ptr: u64, stack: u64, _arg: u64, tls: u64, child_tid: u64) {
    let idx = NUM_THREADS.fetch_add(1, Ordering::SeqCst);
    if idx >= MAX_THREADS {
        crate::serial::raw_str(b"[thread] too many\n");
        return;
    }

    // Build a stack frame for the child that will be "returned to" by
    // context_switch. Push the trampoline address so `ret` jumps there.
    let mut sp = stack;
    sp &= !0xF; // 16-byte align
    sp -= 8;    // space for "return address"
    *(sp as *mut u64) = child_trampoline as u64;

    // Allocate a kernel stack from the bump allocator.
    let kstack_base = KSTACK_NEXT.fetch_add(KSTACK_SIZE, Ordering::Relaxed);
    let kstack_top = kstack_base + KSTACK_SIZE;

    // Allocate a per-thread IST stack for timer interrupts.
    let ist_base = IST_NEXT.fetch_add(IST_STACK_SIZE, Ordering::Relaxed);
    let ist_top = ist_base + IST_STACK_SIZE;

    CONTEXTS[idx] = Context {
        rsp: sp,
        rbx: fn_ptr,
        rbp: 0,
        r12: stack,     // original stack (where arg is at [stack])
        r13: tls,
        r14: child_tid,
        r15: idx as u64,
        valid: true,
        kernel_stack_top: kstack_top,
        ist_stack_top: ist_top,
        sleeping: false,
        futex_addr: 0,
        futex_val: 0,
        tsc_offset: 0,
    };
}

/// Trampoline that runs when we first switch to a child context.
extern "C" fn child_trampoline() -> ! {
    let idx;
    // SAFETY: Reading from static context set up by spawn().
    unsafe {
        // r15 has the thread index (set by context_switch restore)
        let mut thread_idx: u64;
        core::arch::asm!("mov {}, r15", out(reg) thread_idx);
        idx = thread_idx as usize;

        let fn_ptr = CONTEXTS[idx].rbx;
        let stack = CONTEXTS[idx].r12;
        let tls = CONTEXTS[idx].r13;
        let child_tid = CONTEXTS[idx].r14;

        // Set child's TLS (FS_BASE)
        if tls != 0 {
            x86_64::registers::model_specific::Msr::new(0xC000_0100).write(tls);
        }

        // Write child TID
        if child_tid != 0 {
            *(child_tid as *mut u32) = (idx + 1) as u32;
        }

        let arg_on_stack = *(stack as *const u64);
        serial_println!("[thread] child #{} fn={:#x} stack={:#x} arg={:#x}",
            idx, fn_ptr, stack, arg_on_stack);
        // Peek into the arg (pthread struct) to verify start_routine and start_arg
        if arg_on_stack != 0 {
            let start_routine = *(arg_on_stack as *const u64);
            let start_arg = *((arg_on_stack + 8) as *const u64);
            serial_println!("[thread]   pthread: start_routine={:#x} start_arg={:#x}",
                start_routine, start_arg);
        }

        // Replicate musl's __clone child path:
        //   pop rdi    ; arg from child stack
        //   call *r9   ; fn(arg)
        core::arch::asm!(
            "mov rsp, {stack}",
            "xor ebp, ebp",
            "pop rdi",
            "call {func}",
            "mov edi, eax",
            "mov eax, 60",
            "syscall",
            "ud2",
            stack = in(reg) stack,
            func = in(reg) fn_ptr,
            options(noreturn),
        );
    }
}

/// Yield from current context to the next runnable context (round-robin).
pub fn yield_to_other() {
    // Disable interrupts during context switch to prevent the timer
    // from firing mid-switch and corrupting state.
    let were_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();

    let n = NUM_THREADS.load(Ordering::SeqCst);
    if n <= 1 {
        if were_enabled { x86_64::instructions::interrupts::enable(); }
        return;
    }

    let cur = CURRENT.load(Ordering::SeqCst);

    // Find next valid, non-sleeping thread (round-robin)
    let mut next = (cur + 1) % n;
    // SAFETY: single-threaded cooperative access.
    unsafe {
        let mut tries = 0;
        while tries < n {
            if CONTEXTS[next].valid && !CONTEXTS[next].sleeping {
                break;
            }
            next = (next + 1) % n;
            tries += 1;
        }
        if next == cur || !CONTEXTS[next].valid || CONTEXTS[next].sleeping {
            return; // nobody else to switch to (all sleeping or invalid)
        }

        YIELD_COUNT.fetch_add(1, Ordering::Relaxed);

        CURRENT.store(next, Ordering::SeqCst);

        // Switch to the target thread's per-thread kernel stack
        crate::syscall::set_current_kernel_stack(CONTEXTS[next].kernel_stack_top);

        // Update TSS.IST1 to the target thread's IST stack so timer
        // interrupts use a per-thread stack (prevents iretq frame corruption
        // when multiple threads are preempted).
        crate::interrupts::set_ist1(CONTEXTS[next].ist_stack_top);

        context_switch(&raw mut CONTEXTS[cur], &raw const CONTEXTS[next]);
    }
    // Re-enable interrupts after resuming
    if were_enabled { x86_64::instructions::interrupts::enable(); }
}

/// Returns the current thread index (0 = main).
pub fn current_idx() -> usize {
    CURRENT.load(Ordering::Relaxed)
}

/// Returns true if currently executing in a child thread.
pub fn is_child() -> bool {
    CURRENT.load(Ordering::Relaxed) != 0
}

/// Returns true if any child thread has been spawned.
pub fn has_child() -> bool {
    NUM_THREADS.load(Ordering::Relaxed) > 1
}

/// Returns the total number of threads.
pub fn num_threads() -> usize {
    NUM_THREADS.load(Ordering::Relaxed)
}

/// Returns the total yield count.
pub fn yield_count() -> usize {
    YIELD_COUNT.load(Ordering::Relaxed)
}

/// Put the current thread to sleep, waiting on a futex address.
/// The thread won't be scheduled until futex_wake or timer check wakes it.
/// After marking sleeping, yields to the next runnable thread.
pub fn futex_sleep(addr: u64, val: u32) {
    let cur = CURRENT.load(Ordering::SeqCst);
    // SAFETY: cooperative access.
    unsafe {
        CONTEXTS[cur].sleeping = true;
        CONTEXTS[cur].futex_addr = addr;
        CONTEXTS[cur].futex_val = val;
    }
    yield_to_other();
    // When we resume, we've been woken — clear sleeping state
    unsafe {
        CONTEXTS[cur].sleeping = false;
        CONTEXTS[cur].futex_addr = 0;
    }
}

/// Wake up to `count` threads sleeping on the given futex address.
/// Returns the number of threads actually woken.
pub fn futex_wake(addr: u64, count: u32) -> i64 {
    let n = NUM_THREADS.load(Ordering::SeqCst);
    let mut woken = 0i64;
    // SAFETY: cooperative access.
    unsafe {
        for i in 0..n {
            if woken >= count as i64 { break; }
            if CONTEXTS[i].sleeping && CONTEXTS[i].futex_addr == addr {
                CONTEXTS[i].sleeping = false;
                CONTEXTS[i].futex_addr = 0;
                woken += 1;
            }
        }
    }
    woken
}

/// Record a pending wake for an address where WAKE found nobody sleeping.
pub fn mark_futex_pending_wake(addr: u64) {
    let idx = PENDING_COUNT.load(Ordering::Relaxed);
    if idx < MAX_PENDING {
        unsafe { PENDING_WAKES[idx] = addr; }
        PENDING_COUNT.store(idx + 1, Ordering::Relaxed);
    }
}

/// Check all sleeping threads and wake them. Called from timer.
/// Wakes threads whose futex value changed, and also does periodic
/// spurious wakeups (every ~100ms) to prevent deadlocks where all
/// threads sleep and no one advances the state.
pub fn check_futex_waiters() {
    static TICK: AtomicUsize = AtomicUsize::new(0);
    let tick = TICK.fetch_add(1, Ordering::Relaxed);
    // Wake threads whose value changed or that have pending wakes.
    // Every 10th tick (~100ms at 100Hz), do a force-wake as safety net.
    let force_wake = tick % 10 == 0;

    let n = NUM_THREADS.load(Ordering::SeqCst);
    let mut woken = 0;
    // Check pending wakes first: if any address had a WAKE with nobody
    // sleeping, force-wake any thread that's now sleeping on that address.
    let pending = PENDING_COUNT.load(Ordering::Relaxed);

    // SAFETY: cooperative access, interrupts disabled in timer handler path.
    unsafe {
        for i in 0..n {
            if !CONTEXTS[i].sleeping { continue; }
            if CONTEXTS[i].futex_addr == 0 {
                // Timer-sleeping thread (sched_yield): always wake on force_wake
                if force_wake {
                    CONTEXTS[i].sleeping = false;
                    woken += 1;
                }
            } else {
                let addr = CONTEXTS[i].futex_addr;
                let expected = CONTEXTS[i].futex_val;
                let current = *(addr as *const u32);

                // Check if this address has a pending wake
                let mut has_pending = false;
                for p in 0..pending.min(MAX_PENDING) {
                    if PENDING_WAKES[p] == addr {
                        has_pending = true;
                        break;
                    }
                }

                if current != expected || force_wake || has_pending {
                    CONTEXTS[i].sleeping = false;
                    CONTEXTS[i].futex_addr = 0;
                    woken += 1;
                }
            }
        }
        // Clear pending wakes
        if pending > 0 {
            PENDING_COUNT.store(0, Ordering::Relaxed);
        }
    }
    if tick < 20 || (tick % 200 == 0) {
        let mut sleeping_count = 0;
        unsafe {
            for i in 0..n { if CONTEXTS[i].sleeping { sleeping_count += 1; } }
        }
        // Monitor value at thread 2's known futex address 0x4a955914
        let val_at_addr = unsafe {
            if n > 2 {
                *(0x4a955914u64 as *const u32)
            } else { 99 }
        };
        crate::serial_println!("[timer] tick={} woke={} slp={}/{} *0x4a955914={}",
            tick, woken, sleeping_count, n, val_at_addr);
    }
}

/// Put the current thread to sleep until woken by the timer interrupt.
/// Unlike futex_sleep, this doesn't watch any address — it just sleeps.
pub fn sleep_until_timer() {
    let cur = CURRENT.load(Ordering::SeqCst);
    // SAFETY: cooperative access.
    unsafe {
        CONTEXTS[cur].sleeping = true;
        CONTEXTS[cur].futex_addr = 0; // no address — just sleeping
    }
    yield_to_other();
    unsafe {
        CONTEXTS[cur].sleeping = false;
    }
}

/// Low-level context switch.
#[unsafe(naked)]
extern "C" fn context_switch(_from: *mut Context, _to: *const Context) {
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
