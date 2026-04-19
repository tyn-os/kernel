//! Tyn kernel library.

#![no_std]
#![feature(abi_x86_interrupt)]

extern crate alloc;

pub mod drivers;
pub mod interrupts;
pub mod memory;
pub mod net;
pub mod serial;

pub fn halt_loop() -> ! {
    loop { x86_64::instructions::hlt(); }
}
