//! IDT with exception handlers for page faults, GPF, and double faults.

use crate::serial_println;
use spin::Mutex;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

static IDT: Mutex<InterruptDescriptorTable> = Mutex::new(InterruptDescriptorTable::new());

/// Load the IDT with exception handlers.
pub fn init_idt() {
    let mut idt = IDT.lock();
    idt.page_fault.set_handler_fn(page_fault_handler);
    idt.double_fault.set_handler_fn(double_fault_handler);
    idt.general_protection_fault.set_handler_fn(gpf_handler);
    idt.breakpoint.set_handler_fn(breakpoint_handler);
    // SAFETY: The leaked guard keeps the IDT alive for the lifetime of the kernel.
    spin::MutexGuard::leak(idt).load();
}

extern "x86-interrupt" fn page_fault_handler(
    frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    serial_println!(
        "#PF at {:#x}, fault_vaddr={:#x}, err={:?}",
        frame.instruction_pointer,
        Cr2::read_raw(),
        error_code
    );
    crate::halt_loop();
}

extern "x86-interrupt" fn double_fault_handler(
    frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    serial_println!("DOUBLE FAULT at {:#x}", frame.instruction_pointer);
    crate::halt_loop();
}

extern "x86-interrupt" fn gpf_handler(frame: InterruptStackFrame, error_code: u64) {
    serial_println!(
        "#GP at {:#x}, error={:#x}",
        frame.instruction_pointer,
        error_code
    );
    crate::halt_loop();
}

extern "x86-interrupt" fn breakpoint_handler(frame: InterruptStackFrame) {
    serial_println!("BREAKPOINT at {:#x}", frame.instruction_pointer);
}
