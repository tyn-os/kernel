//! Tyn kernel library — core modules for a bare-metal x86_64 microkernel.

#![no_std]
#![feature(abi_x86_interrupt)]
#![feature(naked_functions)]

extern crate alloc;

pub mod acpi;
pub mod apic;
pub mod drivers;
pub mod elf;
pub mod interrupts;
pub mod memory;
pub mod net;
pub mod percpu;
pub mod sched;
pub mod serial;
pub mod smp;
pub mod syscall;
pub mod pipe;
pub mod thread;
pub mod vfs;

/// Halt the CPU in a loop, waking only on interrupts.
pub fn halt_loop() -> ! {
    loop {
        x86_64::instructions::hlt();
    }
}
