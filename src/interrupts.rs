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
        // IPI handler for SMP wakeup (vector 34)
        IDT[34].set_handler_fn(ipi_handler);
        // Spurious interrupt handler for APIC (vector 0xFF)
        IDT[0xFF].set_handler_fn(spurious_handler);
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

extern "x86-interrupt" fn timer_handler(mut frame: InterruptStackFrame) {
    // EOI to APIC (PIC is disabled)
    crate::apic::eoi();

    // Watchdog: every tick (10 ms with the 100 Hz APIC timer). Cheap —
    // iterates at most MAX_THREADS = 24 entries. Needs to be tick-frequent
    // because it doubles as the deadline checker for FUTEX_WAIT timeouts
    // (used by ethr_event_twait → schedulers' timer-aware sleep). With
    // a 1-second cadence, `receive after N` resolution would be 1 s.
    crate::sched::watchdog_wake();

    const KERNEL_BASE: u64 = 0x0F00_0000;
    let ip = frame.instruction_pointer.as_u64();
    if ip < KERNEL_BASE {
        // User-mode code interrupted. Push the original RIP onto the user
        // stack and redirect IRET to a trampoline that does sched_yield.
        // The trampoline does `syscall(sched_yield); ret` — the ret pops
        // the original RIP and resumes user code. check_resched at the
        // syscall exit performs the actual yield.
        crate::sched::timer_tick();

        extern "C" { fn sched_yield_trampoline(); }
        unsafe {
            let user_rsp = frame.stack_pointer.as_u64();
            let new_rsp = user_rsp - 8;
            // Push original RIP onto user stack
            *(new_rsp as *mut u64) = ip;
            frame.as_mut().update(|f| {
                f.instruction_pointer = x86_64::VirtAddr::new(sched_yield_trampoline as u64);
                f.stack_pointer = x86_64::VirtAddr::new(new_rsp);
            });
        }
    } else {
        crate::sched::timer_tick();
    }
}

core::arch::global_asm!(
    ".section .text",
    ".global sched_yield_trampoline",
    "sched_yield_trampoline:",
    // Original RIP is at [rsp] (pushed by timer handler).
    // Save the regs the `syscall` instruction itself clobbers
    // (rax/rcx/r11). The other caller-saved GPRs (rdx/rsi/rdi/r8-r10)
    // are saved and restored by syscall_entry in src/syscall.rs — see
    // BOOT_RELIABILITY.md for the full stack-layout trace.
    "push rax",
    "push rcx",
    "push r11",
    "mov eax, 24",      // SYS_sched_yield
    "syscall",           // → kernel → check_resched → yield
    "pop r11",
    "pop rcx",
    "pop rax",
    "ret",               // pops original RIP → resumes user code
);

extern "x86-interrupt" fn page_fault_handler(
    frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    // Use lock-free serial writes for crash output (works even if another CPU holds the lock)
    crate::serial::raw_str_nolock(b"\n#PF ip=");
    crate::serial::raw_hex_nolock(frame.instruction_pointer.as_u64());
    crate::serial::raw_str_nolock(b" cr2=");
    crate::serial::raw_hex_nolock(Cr2::read_raw());
    crate::serial::raw_str_nolock(b" rsp=");
    crate::serial::raw_hex_nolock(frame.stack_pointer.as_u64());
    crate::serial::raw_str_nolock(b"\n");
    crate::halt_loop();
}

extern "x86-interrupt" fn double_fault_handler(
    _frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    crate::serial::raw_str_nolock(b"\nDOUBLE FAULT\n");
    crate::halt_loop();
}

extern "x86-interrupt" fn gpf_handler(frame: InterruptStackFrame, _error_code: u64) {
    crate::serial::raw_str_nolock(b"\n#GP ip=");
    crate::serial::raw_hex_nolock(frame.instruction_pointer.as_u64());
    crate::serial::raw_str_nolock(b" rsp=");
    crate::serial::raw_hex_nolock(frame.stack_pointer.as_u64());
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
