//! Tyn kernel library — core modules for a bare-metal x86_64 microkernel.

#![no_std]
#![feature(abi_x86_interrupt)]

extern crate alloc;

pub mod drivers;
pub mod interrupts;
pub mod memory;
pub mod net;
pub mod serial;

/// Halt the CPU in a loop, waking only on interrupts.
pub fn halt_loop() -> ! {
    loop {
        x86_64::instructions::hlt();
    }
}
