//! IDT — simple pattern matching rcore's trap.rs.

use crate::serial_println;
use spin::Mutex;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

static IDT: Mutex<InterruptDescriptorTable> = Mutex::new(InterruptDescriptorTable::new());

pub fn init_idt() {
    let mut idt = IDT.lock();

    idt.page_fault.set_handler_fn(page_fault_handler);
    idt.double_fault.set_handler_fn(double_fault_handler);
    idt.general_protection_fault.set_handler_fn(gpf_handler);
    idt.breakpoint.set_handler_fn(breakpoint_handler);

    // Leak the guard so the IDT stays loaded
    spin::MutexGuard::leak(idt).load();
}

extern "x86-interrupt" fn page_fault_handler(
    frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    let cr2 = Cr2::read_raw();
    serial_println!(
        "#PF at {:#x}, fault_vaddr={:#x}, err={:?}",
        frame.instruction_pointer, cr2, error_code
    );
    loop { x86_64::instructions::hlt(); }
}

extern "x86-interrupt" fn double_fault_handler(
    frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    serial_println!("DOUBLE FAULT at {:#x}", frame.instruction_pointer);
    loop { x86_64::instructions::hlt(); }
}

extern "x86-interrupt" fn gpf_handler(
    frame: InterruptStackFrame,
    error_code: u64,
) {
    serial_println!("#GP at {:#x}, error={:#x}", frame.instruction_pointer, error_code);
    loop { x86_64::instructions::hlt(); }
}

extern "x86-interrupt" fn breakpoint_handler(frame: InterruptStackFrame) {
    serial_println!("BREAKPOINT at {:#x}", frame.instruction_pointer);
}
