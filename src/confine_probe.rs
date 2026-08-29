//! Stage 3a confinement teeth (feature `confine_probe`) — the arc's HEADLINE.
//!
//! Proves, by deliberate violation, that a ring-3 write to a **present US=0 kernel
//! address** is (a) DENIED by the MMU as a US-violation (#PF err = U+W+P) and
//! (b) CONTAINED by the kernel (item-4: the fault is caught, the machine does NOT
//! halt, and boot proceeds so BEAM comes up and serves /health) — not merely that
//! "some fault happened".
//!
//! Mechanism: a tiny ring-3 stub (one kernel-`.text` page, temporarily marked US=1)
//! writes a kernel scratch address. The #PF handler, while `armed()`, records the
//! fault and `contain_longjmp`s back into `run()` (an in-kernel setjmp/longjmp) —
//! the illegal access is intercepted, the faulting excursion abandoned, control
//! returned to a safe kernel point. This is exactly how a real kernel delivers a
//! SIGSEGV: the offending instruction is stopped, blast radius bounded, system
//! continues.
//!
//! Mutation control (same run): pass 2 toggles the scratch page US=1. The IDENTICAL
//! ring-3 write now SUCCEEDS (the kernel word actually changes to 0xdead) — so the
//! US bit (enforcement) is proven to be the SOLE difference between "faults +
//! contained" and "writes + corrupts". A guaranteed-US=0 guard page provides the
//! fault-exit for pass 2.
//!
//! OFF by default; build `--features confine_probe`. Runs once at boot, on the boot
//! thread, before BEAM — single-threaded, no concurrency on the shared static.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Page-aligned so each datum owns its 4 KiB page — `set_page_us` on the scratch
/// must not accidentally expose the guard (which stays US=0 to give pass 2 its
/// fault-exit).
#[repr(C, align(4096))]
struct PageWord {
    v: u64,
    _pad: [u8; 4088],
}

/// Kernel-owned scratch (US=0 by default). Ring-3 must NOT be able to write it.
static mut CONFINE_SCRATCH_PG: PageWord = PageWord { v: 0, _pad: [0; 4088] };
/// Always-US=0 guard: the fault-exit for the mutation pass (after the scratch write
/// succeeds, the stub writes here to trigger the contained #PF).
static mut CONFINE_GUARD_PG: PageWord = PageWord { v: 0, _pad: [0; 4088] };

/// Shared excursion record, addressed through a single register (`rcx`) in both the
/// setjmp (`excursion`) and longjmp (`contain_longjmp`) asm — one fixed base avoids
/// the register-allocator placing an operand in a callee-saved reg we manually save.
/// Layout is byte-offset-critical (the asm uses literal `[rcx+N]`):
///   0 rip  8 rsp  16 rbx  24 rbp  32 r12  40 r13  48 r14  56 r15   (jmpbuf)
///   64 entry  72 cs  80 rflags  88 ustk  96 ss                     (iretq frame)
///   104 t1  112 guard  120 val                                     (stub regs)
#[repr(C)]
struct Excursion {
    rip: u64, rsp: u64, rbx: u64, rbp: u64, r12: u64, r13: u64, r14: u64, r15: u64,
    entry: u64, cs: u64, rflags: u64, ustk: u64, ss: u64,
    t1: u64, guard: u64, val: u64,
}
static mut EXC: Excursion = Excursion {
    rip: 0, rsp: 0, rbx: 0, rbp: 0, r12: 0, r13: 0, r14: 0, r15: 0,
    entry: 0, cs: 0, rflags: 0, ustk: 0, ss: 0,
    t1: 0, guard: 0, val: 0,
};

static ARMED: AtomicBool = AtomicBool::new(false);
static FAULT_CR2: AtomicU64 = AtomicU64::new(0);
static FAULT_ERR: AtomicU64 = AtomicU64::new(0);

/// True while a probe excursion is in ring 3 — the #PF handler routes a ring-3 fault
/// to `contain_longjmp` instead of the halt path.
#[inline(always)]
pub fn armed() -> bool {
    ARMED.load(Ordering::Acquire)
}

unsafe extern "C" {
    /// Ring-3 stub: `mov [rsi],rdi ; mov [rdx],rdi ; ud2`. rsi=target1 (scratch),
    /// rdx=target2 (guard, always US=0), rdi=value.
    fn confine_stub();
}

core::arch::global_asm!(
    ".section .text",
    ".global confine_stub",
    "confine_stub:",
    "mov [rsi], rdi", // pass1: scratch US=0 → #PF here; pass2: scratch US=1 → writes
    "mov [rdx], rdi", // pass2 fault-exit: guard is always US=0 → #PF here
    "ud2",            // unreached (safety net if neither faulted)
);

/// Called from `page_fault_handler` on a ring-3 fault while `armed()`. Records the
/// fault, disarms, swaps back to the kernel GS base (the CPU did NOT swapgs on the
/// ring-3 fault), then longjmps to the recovery point captured in `excursion`. Never
/// returns to the interrupt handler — the excursion is abandoned, the kernel resumes
/// on its own stack with callee-saved + rsp restored. THIS is "contain, not halt".
#[inline(never)]
pub unsafe fn contain_longjmp(cr2: u64, err: u64) -> ! {
    FAULT_CR2.store(cr2, Ordering::Release);
    FAULT_ERR.store(err, Ordering::Release);
    ARMED.store(false, Ordering::Release);
    unsafe {
        core::arch::asm!(
            "swapgs",              // ring-3 fault entered on user GS → restore kernel GS
            "mov rbx, [rcx+16]",
            "mov rbp, [rcx+24]",
            "mov r12, [rcx+32]",
            "mov r13, [rcx+40]",
            "mov r14, [rcx+48]",
            "mov r15, [rcx+56]",
            "mov rsp, [rcx+8]",
            "jmp [rcx]",
            in("rcx") core::ptr::addr_of_mut!(EXC),
            options(noreturn),
        );
    }
}

/// One ring-3 excursion: setjmp (save recovery), iretq into the stub, and "return"
/// here via `contain_longjmp` when the stub faults. `target1` → rsi, `value` → rdi;
/// rdx is always the US=0 guard (pass-2 fault-exit).
#[inline(never)]
unsafe fn excursion(target1: u64, value: u64) {
    let (ucode, udata) = crate::percpu::user_selectors();
    unsafe {
        EXC.entry = confine_stub as usize as u64;
        EXC.cs = (ucode | 3) as u64;
        EXC.rflags = 0x202; // IF=1 + reserved
        EXC.ustk = 0x001F_0000; // top of the low-2 MiB US=1 identity page (stub uses no stack)
        EXC.ss = (udata | 3) as u64;
        EXC.t1 = target1;
        EXC.guard = core::ptr::addr_of_mut!(CONFINE_GUARD_PG.v) as u64;
        EXC.val = value;
    }
    ARMED.store(true, Ordering::Release);
    unsafe {
        core::arch::asm!(
            // --- setjmp: capture recovery RIP (label 3), rsp, callee-saved into EXC ---
            "lea rax, [rip + 3f]",
            "mov [rcx+0],  rax",
            "mov [rcx+8],  rsp",
            "mov [rcx+16], rbx",
            "mov [rcx+24], rbp",
            "mov [rcx+32], r12",
            "mov [rcx+40], r13",
            "mov [rcx+48], r14",
            "mov [rcx+56], r15",
            // --- excursion: build the ring-3 iretq frame from EXC and go ---
            "cli",
            "swapgs",
            "push qword ptr [rcx+96]", // ss
            "push qword ptr [rcx+88]", // ustk
            "push qword ptr [rcx+80]", // rflags
            "push qword ptr [rcx+72]", // cs
            "push qword ptr [rcx+64]", // entry
            "mov rsi, [rcx+104]",      // t1  → stub rsi
            "mov rdx, [rcx+112]",      // guard → stub rdx
            "mov rdi, [rcx+120]",      // val  → stub rdi
            "iretq",
            // --- recovery: contain_longjmp jmps here with rsp+callee-saved restored ---
            "3:",
            in("rcx") core::ptr::addr_of_mut!(EXC),
            // rax = tmp (lea); the excursion clobbers all caller-saved via the ring-3
            // detour + longjmp — callee-saved (rbx,rbp,r12-15,rsp) are saved above and
            // restored by contain_longjmp, so Rust sees them preserved.
            out("rax") _, lateout("rdx") _, lateout("rsi") _, lateout("rdi") _,
            lateout("r8") _, lateout("r9") _, lateout("r10") _, lateout("r11") _,
        );
    }
}

/// Run the confinement teeth. Call once at boot, on the boot thread, AFTER paging +
/// GDT/TSS (RSP0) + syscall MSRs are live and the IDT #PF handler is installed, and
/// BEFORE BEAM is launched. Boot then continues → BEAM serves /health, the liveness
/// witness that the contained fault did not take the kernel down.
pub unsafe fn run() {
    let scratch = unsafe { core::ptr::addr_of_mut!(CONFINE_SCRATCH_PG.v) as u64 };
    let guard = unsafe { core::ptr::addr_of_mut!(CONFINE_GUARD_PG.v) as u64 };
    let stub = confine_stub as usize as u64;
    crate::serial_println!(
        "[confine] T-confine: ring-3 write to US=0 kernel mem. stub={:#x} scratch={:#x} guard={:#x}",
        stub, scratch, guard
    );
    unsafe {
        // Make the stub ring-3-executable (its page + the next, in case it straddles).
        crate::memory::paging::set_page_us(stub, true);
        crate::memory::paging::set_page_us(stub + 0x1000, true);

        // ---- Pass 1: ENFORCED. scratch is US=0. Ring-3 write must fault + contain. ----
        core::ptr::write_volatile(scratch as *mut u64, 0);
        FAULT_CR2.store(0, Ordering::Release);
        FAULT_ERR.store(0, Ordering::Release);
        excursion(scratch, 0x00C0_FFEE);
        let e_cr2 = FAULT_CR2.load(Ordering::Acquire);
        let e_err = FAULT_ERR.load(Ordering::Acquire);
        let e_scratch = core::ptr::read_volatile(scratch as *const u64);
        crate::serial_println!(
            "[confine] ENFORCED : #PF cr2={:#x} err={:#x} scratch-after={:#x}  (want cr2={:#x}, err=0x7 U+W+P, scratch=0 → DENIED)",
            e_cr2, e_err, e_scratch, scratch
        );

        // ---- Pass 2: MUTATION control. scratch US=1 → SAME write succeeds. ----
        crate::memory::paging::set_page_us(scratch, true);
        FAULT_CR2.store(0, Ordering::Release);
        FAULT_ERR.store(0, Ordering::Release);
        excursion(scratch, 0x0000_DEAD);
        let m_cr2 = FAULT_CR2.load(Ordering::Acquire);
        let m_scratch = core::ptr::read_volatile(scratch as *const u64);
        crate::serial_println!(
            "[confine] MUTATION : scratch US=1 → scratch-after={:#x}, then guard #PF cr2={:#x}  (want scratch=0xdead → WRITES, cr2={:#x})",
            m_scratch, m_cr2, guard
        );

        // Restore enforcement + un-expose the stub.
        crate::memory::paging::set_page_us(scratch, false);
        crate::memory::paging::set_page_us(stub, false);
        crate::memory::paging::set_page_us(stub + 0x1000, false);
        core::arch::asm!("sti"); // re-enable interrupts (the excursion cli'd)

        // ---- Verdict ----
        let us_violation = (e_err & 0x1) != 0 && (e_err & 0x2) != 0 && (e_err & 0x4) != 0;
        let denied = e_scratch == 0 && e_cr2 == scratch;
        let mutated = m_scratch == 0x0000_DEAD && m_cr2 == guard;
        if us_violation && denied && mutated {
            crate::serial_println!(
                "[confine] VERDICT: PASS — ring-3 kernel-write DENIED as US-violation + CONTAINED (kernel alive); enforcement (US bit) is the sole difference (mutation proved). T-CONFINE OK."
            );
        } else {
            crate::serial_println!(
                "[confine] VERDICT: FAIL us_violation={} denied={} mutated={}",
                us_violation, denied, mutated
            );
        }
    }
}
