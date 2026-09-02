//! IDT with exception handlers, timer interrupt with IST.

use crate::serial_println;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

/// Shared IDT — initialized once by BSP, loaded on all CPUs.
/// Not behind a Mutex because it's write-once then read-only.
static mut IDT: InterruptDescriptorTable = InterruptDescriptorTable::new();

/// Set up the shared IDT and per-CPU GDT/TSS for the BSP.
/// APs call `load_idt()` after their own GDT/TSS is set up via `percpu::init_cpu`.
pub fn init_idt() {
    // Initialize per-CPU GDT+TSS for BSP (cpu 0, apic 0)
    crate::percpu::init_cpu(0, 0);

    // Set up the shared IDT (only BSP writes to it, before APs exist)
    unsafe {
        IDT.page_fault.set_handler_fn(page_fault_handler);
        IDT.double_fault.set_handler_fn(double_fault_handler);
        IDT.general_protection_fault.set_handler_fn(gpf_handler);
        IDT.breakpoint.set_handler_fn(breakpoint_handler);
        IDT.invalid_opcode.set_handler_fn(invalid_opcode_handler);
        IDT.device_not_available.set_handler_fn(device_not_available_handler);
        IDT.simd_floating_point.set_handler_fn(simd_handler);
        // Timer at vector 32 with IST1 (safe dedicated stack for the timer ISR).
        IDT[32].set_handler_fn(timer_handler)
            .set_stack_index(0);
        // IPI handler for SMP wakeup (vector 34). BUG-1 Cut-2: MUST use the same IST
        // as the timer. Tyn runs ring 0, so a vector WITHOUT an IST pushes its CPU
        // interrupt frame onto the *current* stack. If this IPI preempts BeamAsm user
        // code running on the user stack — an idle CPU can transition idle→running-user
        // between sched.rs send_ipi's is_idle check and IPI delivery — the CPU writes
        // the 40-byte frame into the user SysV red zone [rsp-128..rsp] and clobbers
        // leaf spills → transient wrong md5. SMP-ONLY (IPIs need >1 CPU) ⇒ exactly
        // BUG-1's -smp2 residual, and consistent with Cut 1 (XMM clean — this clobbers
        // GPR spills, not XMM). Path A only IST'd the timer, missing this vector.
        // Sharing IST1 with the timer is safe: interrupt gates run IF=0 so timer/IPI
        // can't nest on one CPU, and each CPU has its own IST via percpu::init_cpu.
        IDT[34].set_handler_fn(ipi_handler)
            .set_stack_index(0);
        // Spurious interrupt handler for APIC (vector 0xFF)
        IDT[0xFF].set_handler_fn(spurious_handler);

        // Isolation Stage 2: DPL=3 int-gates for the ring-3 transition shim, so a
        // ring-3 `int 0x80/0x81` is permitted to invoke them (a DPL=0 gate would
        // #GP the ring-3 int — that mis-DPL is one of the mutation-teeth). Handlers
        // are naked global_asm (swapgs/iretq); interrupt gates run IF=0. Feature-
        // gated — never in production.
        #[cfg(feature = "stage2_shim")]
        {
            extern "C" {
                fn stage2_int80_entry();
                fn stage2_int81_entry();
            }
            IDT[0x80]
                .set_handler_addr(x86_64::VirtAddr::new(stage2_int80_entry as u64))
                .set_privilege_level(x86_64::PrivilegeLevel::Ring3);
            IDT[0x81]
                .set_handler_addr(x86_64::VirtAddr::new(stage2_int81_entry as u64))
                .set_privilege_level(x86_64::PrivilegeLevel::Ring3);
        }

        IDT.load_unsafe();
    }
}

/// Update TSS.IST1 to the given stack top address. Called during context
/// switch so each thread gets its own timer interrupt stack.
/// With per-CPU TSS (SMP), this is handled differently — each CPU has
/// its own IST via percpu::init_cpu. This is kept for OTP 20 compatibility.
pub fn set_ist1(_stack_top: u64) {
    // Per-CPU TSS handles IST stacks now. This is a no-op.
    // For SMP, each CPU's TSS.IST1 is set in percpu::init_cpu.
}

/// Load the shared IDT on the current CPU. Called by APs after percpu::init_cpu.
pub fn load_idt() {
    unsafe { IDT.load_unsafe(); }
}

/// Initialize the PIT (Programmable Interval Timer) at ~100 Hz.
/// Also set up the PIC to deliver IRQ0 at vector 32.
pub fn init_timer() {
    unsafe {
        // Remap PIC: IRQ0-7 → vectors 32-39, IRQ8-15 → vectors 40-47
        // ICW1: start init, cascade mode, ICW4 needed
        x86_64::instructions::port::Port::<u8>::new(0x20).write(0x11);
        x86_64::instructions::port::Port::<u8>::new(0xA0).write(0x11);
        // ICW2: vector offsets
        x86_64::instructions::port::Port::<u8>::new(0x21).write(32);
        x86_64::instructions::port::Port::<u8>::new(0xA1).write(40);
        // ICW3: cascading
        x86_64::instructions::port::Port::<u8>::new(0x21).write(4);
        x86_64::instructions::port::Port::<u8>::new(0xA1).write(2);
        // ICW4: 8086 mode
        x86_64::instructions::port::Port::<u8>::new(0x21).write(0x01);
        x86_64::instructions::port::Port::<u8>::new(0xA1).write(0x01);
        // Mask all except IRQ0 (timer)
        x86_64::instructions::port::Port::<u8>::new(0x21).write(0xFE); // unmask IRQ0
        x86_64::instructions::port::Port::<u8>::new(0xA1).write(0xFF); // mask all slave

        // Program PIT channel 0 for ~100 Hz (divisor = 11932 = 0x2E9C)
        // Higher frequency gives ERTS more preemption slots for thread-progress.
        // The binary is patched to skip monotonic time backwards checks.
        x86_64::instructions::port::Port::<u8>::new(0x43).write(0x36);
        x86_64::instructions::port::Port::<u8>::new(0x40).write(0x9C); // low byte of 11932
        x86_64::instructions::port::Port::<u8>::new(0x40).write(0x2E); // high byte of 11932
    }
    // Clear any stale IRQs with EOI before enabling interrupts
    unsafe {
        x86_64::instructions::port::Port::<u8>::new(0x20).write(0x20);
        x86_64::instructions::port::Port::<u8>::new(0xA0).write(0x20);
    }
    // Set the timer_active flag so the syscall exit path knows to sti.
    unsafe {
        extern "C" { static mut timer_active: u8; }
        timer_active = 1;
    }
    // Enable interrupts
    x86_64::instructions::interrupts::enable();
}

/// Stage 3a: on a ring3→ring0 interrupt/trap the CPU does NOT swap GS, so the
/// handler would run with BEAM's user GS base and mis-read per-CPU data (`gs:[0]`
/// etc.). Swap iff the interrupt came from ring 3 (saved CS.RPL == 3); pair each
/// call with `swapgs_restore` before returning to ring 3. While BEAM is still ring 0
/// (pre-flip) every interrupt is CS.RPL==0 → both are no-ops, so this is inert.
#[inline(always)]
fn from_ring3(frame: &InterruptStackFrame) -> bool {
    (frame.code_segment & 3) == 3
}
#[inline(always)]
unsafe fn swapgs_enter(frame: &InterruptStackFrame) -> bool {
    let u = from_ring3(frame);
    if u {
        core::arch::asm!("swapgs", options(nomem, nostack, preserves_flags));
    }
    u
}
#[inline(always)]
unsafe fn swapgs_restore(was_user: bool) {
    if was_user {
        core::arch::asm!("swapgs", options(nomem, nostack, preserves_flags));
    }
}

extern "x86-interrupt" fn timer_handler(mut frame: InterruptStackFrame) {
    // Stage 3a: correct GS if we preempted ring-3 BEAM. (No-op while BEAM is ring 0.)
    let from_user_ring = unsafe { swapgs_enter(&frame) };
    // EOI to APIC (PIC is disabled)
    crate::apic::eoi();

    // Watchdog: every tick (10 ms with the 100 Hz APIC timer). Cheap —
    // iterates at most MAX_THREADS = 24 entries. Needs to be tick-frequent
    // because it doubles as the deadline checker for FUTEX_WAIT timeouts
    // (used by ethr_event_twait → schedulers' timer-aware sleep). With
    // a 1-second cadence, `receive after N` resolution would be 1 s.
    crate::sched::watchdog_wake();

    const KERNEL_BASE: u64 = 0x0F00_0000;
    // JIT/mmap region: BeamAsm emits native code into mmap'd pages, which
    // live above the static ELF segments (MMAP_NEXT base = 0x1A00_0000).
    // Treat those addresses as user code too — without this, timer ticks
    // that land inside JIT'd functions only set NEED_RESCHED and never
    // run the preemption trampoline, so a hot JIT loop can starve the
    // scheduler entirely.
    const MMAP_BASE: u64 = 0x1A00_0000;
    const MMAP_END: u64 = 0xA000_0000;
    let ip = frame.instruction_pointer.as_u64();
    let is_user = ip < KERNEL_BASE || (ip >= MMAP_BASE && ip < MMAP_END);
    // Stage 3a increment-1: the BUG-1 Path-A trampoline injects KERNEL code into the
    // interrupted flow — impossible for ring-3 BEAM (can't run kernel code in ring 3).
    // So take the trampoline ONLY for a ring-0 interruptee (pre-flip / never post-
    // flip); a ring-3 preemption just ticks (defers the reschedule to the scheduler
    // via the existing cooperative-yield path). Increment-2 replaces this with the
    // direct Kind-B context switch on the clean RSP0 stack (dissolves BUG-1).
    if is_user && !from_user_ring {
        // User (JIT/beam.smp) code interrupted. Redirect IRET to a trampoline that
        // does sched_yield, then resumes the interrupted code. check_resched at the
        // syscall exit performs the actual yield.
        crate::sched::timer_tick();

        extern "C" { fn sched_yield_trampoline(); }
        unsafe {
            // BUG-1 fix (Path A): the trampoline's context goes in a per-thread
            // PREEMPT REGION reserved ABOVE the kernel-stack top (gs:[0]), NOT on
            // the user stack — the interrupted thread's SysV red zone
            // [user_rsp-1..-128] is never touched (the old bug wrote its return
            // frame there and clobbered BeamAsm leaf spills → wrong md5/bincopy).
            // The region is reserved by the two live kstack allocators
            // (syscall_stack_0's dead-neighbor space; sched.rs KSTACK_NEXT's
            // +PREEMPT_REGION_SIZE bump — see docs/STACK_ALLOCATOR_INVENTORY.md),
            // and located here via gs:[0] (the current thread's kstack top).
            //
            // The trampoline runs with IF=0 (we clear IF in the frame we iretq into
            // it), so no nested timer preemption → a single per-thread frame
            // suffices. It ends in `iretq`, atomically restoring orig RIP + user_rsp
            // + the interrupted RFLAGS (IF=1). The syscall's `mov rsp, gs:[0]` uses
            // the kernel stack BELOW gs:[0], never the region above it.
            let kstack_top: u64;
            core::arch::asm!("mov {}, gs:[0]", out(reg) kstack_top,
                             options(nostack, preserves_flags));
            let region_top = kstack_top + PREEMPT_REGION_SIZE;
            let orig_rflags = frame.cpu_flags;      // IF=1 (interrupted code)
            let cs = frame.code_segment;
            let ss = frame.stack_segment;
            let user_rsp = frame.stack_pointer.as_u64();
            // iretq frame at [region_top-40 .. region_top]:
            //   [rsp]=orig_rip, +8=cs, +16=rflags, +24=user_rsp, +32=ss
            let f = (region_top - 40) as *mut u64;
            *f.add(0) = ip;            // orig RIP
            *f.add(1) = cs;
            *f.add(2) = orig_rflags;   // resume with interrupts enabled
            *f.add(3) = user_rsp;
            *f.add(4) = ss;
            frame.as_mut().update(|fr| {
                fr.instruction_pointer = x86_64::VirtAddr::new(sched_yield_trampoline as u64);
                fr.stack_pointer = x86_64::VirtAddr::new(region_top - 40);
                fr.cpu_flags = orig_rflags & !(1u64 << 9); // clear IF → trampoline runs IF=0
            });
        }
    } else {
        crate::sched::timer_tick();
    }
    // Stage 3a: restore user GS before iretq back to ring 3 (no-op when ring 0).
    // NOTE (phase 2): the ring-3 preemption path above (trampoline) still assumes
    // ring-0 preemption — it must be reworked to context-switch directly on the
    // clean RSP0 stack (which also dissolves BUG-1). Inert here: from_user_ring is
    // false while BEAM is ring 0.
    unsafe { swapgs_restore(from_user_ring); }
}

/// Per-thread preemption-context region reserved ABOVE each kernel-stack top
/// (gs:[0]). BUG-1 Path A builds its iretq frame + syscall-clobbered-reg saves
/// here (64 B used; rest is headroom/alignment). Must be reserved by EVERY live
/// kernel-stack allocator — see docs/STACK_ALLOCATOR_INVENTORY.md.
pub const PREEMPT_REGION_SIZE: u64 = 256;

core::arch::global_asm!(
    ".section .text",
    ".global sched_yield_trampoline",
    "sched_yield_trampoline:",
    // BUG-1 fix (Path A): entered by the timer handler with rsp pointing at an
    // iretq frame [orig_rip, cs, rflags(IF=1), user_rsp, ss] in the per-thread
    // PREEMPT REGION (above gs:[0]), and IF=0 (no nested preemption). Save the regs
    // the `syscall` instruction clobbers (rax/rcx/r11) BELOW that frame — the other
    // caller-saved GPRs (rdx/rsi/rdi/r8-r10) are handled by syscall_entry. The
    // syscall's `mov rsp, gs:[0]` switches to the kernel stack BELOW the region, so
    // neither the region nor the user red zone is touched.
    "push rax",
    "push rcx",
    "push r11",
    "mov eax, 24",      // SYS_sched_yield
    "syscall",           // → kernel → check_resched → yield
    "pop r11",
    "pop rcx",
    "pop rax",
    // iretq atomically restores orig_rip + user_rsp + interrupted RFLAGS(IF=1),
    // resuming the interrupted code exactly where it was — user stack untouched.
    "iretq",
);

/// Stage 3b.2 SMAP hunt: dedup table so each unwrapped copy site (by faulting IP) logs
/// once across a run. Feature-gated — the hunt instrument only.
#[cfg(feature = "smap_hunt")]
mod smap_hunt {
    use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    const MAX: usize = 128;
    static SEEN_IP: [AtomicU64; MAX] = [const { AtomicU64::new(0) }; MAX];
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    /// Returns true if `ip` is a newly-seen site (caller logs it).
    pub fn note(ip: u64) -> bool {
        let n = COUNT.load(Ordering::Acquire).min(MAX);
        for i in 0..n {
            if SEEN_IP[i].load(Ordering::Relaxed) == ip {
                return false;
            }
        }
        let idx = COUNT.fetch_add(1, Ordering::AcqRel);
        if idx < MAX {
            SEEN_IP[idx].store(ip, Ordering::Relaxed);
        }
        true
    }
}

#[allow(unused_mut)] // `mut frame` is only mutated under feature `smap_hunt`
extern "x86-interrupt" fn page_fault_handler(
    mut frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    // Stage 3a confinement teeth (feature): an armed ring-3 fault IS the deliberate
    // probe writing a US=0 kernel address. Record it and longjmp back into the probe
    // (item-4: contain, not halt). Never returns to this handler.
    #[cfg(feature = "confine_probe")]
    {
        if from_ring3(&frame) && crate::confine_probe::armed() {
            unsafe {
                crate::confine_probe::contain_longjmp(Cr2::read_raw(), error_code.bits());
            }
        }
    }
    // Stage 3b.0: a stray ring-3 access to the low 2 MiB is a latent ERTS NULL-base RMW
    // (ring 0 masked it; ring 3 faults). Satisfy it with a dedicated writable US=1 scratch
    // page — BEAM's read/write lands there, kernel low-RAM stays confined (US=0) — and
    // RETRY. Read OR write (the ERTS path reads 0x2be4 then writes it). Any non-low fault
    // falls through to report + contain. Log each unique page (self-deduping — a mapped
    // page no longer faults).
    if from_ring3(&frame) {
        let cr2 = Cr2::read_raw();
        if cr2 < 0x20_0000 && unsafe { crate::memory::paging::map_low_scratch(cr2) } {
            crate::serial::raw_str_nolock(b"[low-zero] ring-3 low access cr2=");
            crate::serial::raw_hex_nolock(cr2);
            crate::serial::raw_str_nolock(b" ip=");
            crate::serial::raw_hex_nolock(frame.instruction_pointer.as_u64());
            crate::serial::raw_str_nolock(b" err=");
            crate::serial::raw_hex_nolock(error_code.bits());
            crate::serial::raw_str_nolock(b"\n");
            return; // retry (now hits the scratch page)
        }
    }
    // Stage 3b.2 SMAP hunt (feature): a ring-0 access (supervisor, U/S=0) to a PRESENT,
    // US=1 page with AC=0 is an UNWRAPPED copy site. Log it (once per faulting IP), set AC
    // in the interrupted RFLAGS so the retried access proceeds, and return — so ONE run
    // enumerates every site the exercised workload reaches instead of halting at the first.
    // (This is the enumeration instrument; real enforcement is the uaccess guard.)
    #[cfg(feature = "smap_hunt")]
    {
        let err = error_code.bits();
        let cr2 = Cr2::read_raw();
        if !from_ring3(&frame)
            && (err & 0x1) != 0
            && (err & 0x4) == 0
            && crate::memory::paging::user_accessible(cr2, 1)
        {
            let ip = frame.instruction_pointer.as_u64();
            if smap_hunt::note(ip) {
                // ret = the value at the interrupted RSP. For a leaf routine
                // (compiler-builtins memcpy/memset use `rep movs`, no prologue push) this
                // is the CALLER's return address — which pins the bulk-copy call site the
                // faulting ip (inside memcpy/memset) can't. Reading the kernel stack (US=0)
                // from ring 0 is safe.
                let ret = unsafe { *(frame.stack_pointer.as_u64() as *const u64) };
                crate::serial::raw_str_nolock(b"[smap-site] ip=");
                crate::serial::raw_hex_nolock(ip);
                crate::serial::raw_str_nolock(b" cr2=");
                crate::serial::raw_hex_nolock(cr2);
                crate::serial::raw_str_nolock(b" err=");
                crate::serial::raw_hex_nolock(err);
                crate::serial::raw_str_nolock(b" ret=");
                crate::serial::raw_hex_nolock(ret);
                crate::serial::raw_str_nolock(b"\n");
            }
            unsafe { frame.as_mut().update(|f| f.cpu_flags |= 1 << 18) }; // set AC → retried access proceeds
            return;
        }
    }
    // Fault report (lock-free serial: works even if another CPU holds the lock). The
    // cs+err pair classifies it: cs.RPL==3 ⇒ ring-3 (BEAM); err bit0=P bit1=W bit2=U
    // bit4=I/D — e.g. err=0x7 (P+W+U) is a ring-3 write to a US=0 kernel page (a
    // confinement violation). This is item-4's report half — knowing WHAT faulted is
    // useful for production containment, so it stays (the verbose GPR/fs debug dump
    // used to root-cause the ring-3 startup faults was trimmed).
    crate::serial::raw_str_nolock(b"\n#PF ip=");
    crate::serial::raw_hex_nolock(frame.instruction_pointer.as_u64());
    crate::serial::raw_str_nolock(b" cr2=");
    crate::serial::raw_hex_nolock(Cr2::read_raw());
    crate::serial::raw_str_nolock(b" rsp=");
    crate::serial::raw_hex_nolock(frame.stack_pointer.as_u64());
    crate::serial::raw_str_nolock(b" cs=");
    crate::serial::raw_hex_nolock(frame.code_segment);
    crate::serial::raw_str_nolock(b" err=");
    crate::serial::raw_hex_nolock(error_code.bits());
    crate::serial::raw_str_nolock(b"\n");
    // Item-4: contain-not-halt for a ring-3 fault. Kill the faulting context and hand
    // control to the scheduler so the kernel + other threads survive — an unexpected
    // ring-3 fault (e.g. mid Nitro md5-SMP hammer) reports+contains instead of a silent
    // machine halt. A ring-0 fault is a kernel bug with no safe containment → halt.
    if from_ring3(&frame) {
        unsafe { core::arch::asm!("swapgs"); } // ring-3 entered on user GS → restore kernel GS
        crate::serial::raw_str_nolock(b"[contain] ring-3 fault: killing faulting thread; kernel continues\n");
        crate::sched::thread_exit(); // marks current Dead + switches to scheduler; never returns
    }
    crate::halt_loop();
}

extern "x86-interrupt" fn double_fault_handler(
    _frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    crate::serial::raw_str_nolock(b"\nDOUBLE FAULT\n");
    crate::halt_loop();
}

extern "x86-interrupt" fn gpf_handler(frame: InterruptStackFrame, error_code: u64) {
    // Capture caller-saved GPRs at the very top — Rust hasn't generated
    // any code to clobber them yet, so these reads see the actual
    // register state at the time of the fault. Did this once before
    // to diagnose the BOOT_STALL_TSE corruption (then it was _dl_ns
    // showing a non-canonical pointer); doing it again to see what
    // value is in rcx at the JIT _int_malloc fault.
    let rax: u64;
    let rcx: u64;
    let rdx: u64;
    let r14: u64;
    unsafe {
        core::arch::asm!("mov {}, rax", out(reg) rax, options(nostack, preserves_flags));
        core::arch::asm!("mov {}, rcx", out(reg) rcx, options(nostack, preserves_flags));
        core::arch::asm!("mov {}, rdx", out(reg) rdx, options(nostack, preserves_flags));
        core::arch::asm!("mov {}, r14", out(reg) r14, options(nostack, preserves_flags));
    }
    crate::serial::raw_str_nolock(b"\n#GP ip=");
    crate::serial::raw_hex_nolock(frame.instruction_pointer.as_u64());
    crate::serial::raw_str_nolock(b" rsp=");
    crate::serial::raw_hex_nolock(frame.stack_pointer.as_u64());
    crate::serial::raw_str_nolock(b" err=");
    crate::serial::raw_hex_nolock(error_code);
    crate::serial::raw_str_nolock(b" rax=");
    crate::serial::raw_hex_nolock(rax);
    crate::serial::raw_str_nolock(b" rcx=");
    crate::serial::raw_hex_nolock(rcx);
    crate::serial::raw_str_nolock(b" rdx=");
    crate::serial::raw_hex_nolock(rdx);
    crate::serial::raw_str_nolock(b" r14=");
    crate::serial::raw_hex_nolock(r14);
    crate::serial::raw_str_nolock(b"\n");
    crate::halt_loop();
}

extern "x86-interrupt" fn breakpoint_handler(_frame: InterruptStackFrame) {}

extern "x86-interrupt" fn invalid_opcode_handler(frame: InterruptStackFrame) {
    crate::serial::raw_str_nolock(b"\n#UD ip=");
    crate::serial::raw_hex_nolock(frame.instruction_pointer.as_u64());
    crate::serial::raw_str_nolock(b"\n");
    crate::halt_loop();
}

extern "x86-interrupt" fn device_not_available_handler(_frame: InterruptStackFrame) {
    crate::serial::raw_str(b"#NM\n");
    crate::halt_loop();
}

extern "x86-interrupt" fn simd_handler(_frame: InterruptStackFrame) {
    crate::serial::raw_str(b"#XM\n");
    crate::halt_loop();
}

extern "x86-interrupt" fn ipi_handler(_frame: InterruptStackFrame) {
    // Just EOI — no serial output to keep the handler minimal
    crate::apic::eoi();
}

extern "x86-interrupt" fn spurious_handler(_frame: InterruptStackFrame) {
    // Spurious interrupts from the APIC — no EOI needed
}
