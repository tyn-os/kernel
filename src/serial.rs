//! Serial port driver using UART 16550 on COM1.
//!
//! All kernel logging goes through serial output, which QEMU
//! captures via `-serial stdio` for headless operation.

use spin::{Lazy, Mutex};
use uart_16550::SerialPort;

/// Global serial port instance on COM1 (I/O port 0x3F8).
pub static SERIAL1: Lazy<Mutex<SerialPort>> = Lazy::new(|| {
    // SAFETY: 0x3F8 is the standard COM1 I/O port address.
    let mut serial_port = unsafe { SerialPort::new(0x3F8) };
    serial_port.init();
    Mutex::new(serial_port)
});

#[doc(hidden)]
pub fn _print(args: ::core::fmt::Arguments) {
    use core::fmt::Write;
    use x86_64::instructions::interrupts;

    // Disable interrupts while holding the serial lock to prevent deadlock
    // if an interrupt handler also tries to print.
    interrupts::without_interrupts(|| {
        SERIAL1
            .lock()
            .write_fmt(args)
            .expect("printing to serial failed");
    });
}

/// Prints to the serial port (COM1).
#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => {
        $crate::serial::_print(format_args!($($arg)*))
    };
}

/// Write a hex u64 value to COM1 without format machinery.
pub fn raw_hex(val: u64) {
    let hex = b"0123456789abcdef";
    raw_str(b"0x");
    let mut started = false;
    for i in (0..16).rev() {
        let nibble = ((val >> (i * 4)) & 0xf) as usize;
        if nibble != 0 || started || i == 0 {
            started = true;
            // SAFETY: COM1 I/O ports.
            unsafe {
                while (x86_64::instructions::port::Port::<u8>::new(0x3FD).read() & 0x20) == 0 {}
                x86_64::instructions::port::Port::<u8>::new(0x3F8).write(hex[nibble]);
            }
        }
    }
}

/// Write a raw byte string to COM1 without format machinery.
/// Safe to call even when .rodata vtables are corrupted.
pub fn raw_str(s: &[u8]) {
    for &b in s {
        // SAFETY: 0x3F8 is COM1 data port, 0x3FD is LSR.
        unsafe {
            while (x86_64::instructions::port::Port::<u8>::new(0x3FD).read() & 0x20) == 0 {}
            x86_64::instructions::port::Port::<u8>::new(0x3F8).write(b);
        }
    }
}

/// Prints to the serial port (COM1) with a newline.
#[macro_export]
macro_rules! serial_println {
    () => ($crate::serial_print!("\n"));
    ($fmt:expr) => ($crate::serial_print!(concat!($fmt, "\n")));
    ($fmt:expr, $($arg:tt)*) => ($crate::serial_print!(
        concat!($fmt, "\n"), $($arg)*
    ));
}
