//! Serial port driver using UART 16550 on COM1.
//!
//! All kernel logging goes through serial output, which QEMU
//! captures via `-serial stdio` for headless operation.

use core::sync::atomic::{AtomicBool, Ordering};
use spin::{Lazy, Mutex};
use uart_16550::SerialPort;

/// Global serial port instance on COM1 (I/O port 0x3F8).
pub static SERIAL1: Lazy<Mutex<SerialPort>> = Lazy::new(|| {
    // SAFETY: 0x3F8 is the standard COM1 I/O port address.
    let mut serial_port = unsafe { SerialPort::new(0x3F8) };
    serial_port.init();
    Mutex::new(serial_port)
});

/// When set, routine kernel logging via `serial_print!`/`serial_println!` is
/// suppressed. Flipped on once boot completes (the BEAM prints
/// `serial_shell ready`) so the serial-console eval shell isn't garbled by
/// `[vfs]`/`[net]` debug logs. Panic and fault handlers call `set_quiet(false)`
/// first, so faults are always printed. Boot logs (which aid diagnosing the
/// cold-boot stall) stay visible until boot actually succeeds.
static QUIET: AtomicBool = AtomicBool::new(false);

/// Suppress (`true`) or restore (`false`) routine kernel serial logging.
pub fn set_quiet(q: bool) {
    QUIET.store(q, Ordering::Relaxed);
}

/// Whether routine kernel logging is currently suppressed.
pub fn is_quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

#[doc(hidden)]
pub fn _print(args: ::core::fmt::Arguments) {
    use core::fmt::Write;
    use x86_64::instructions::interrupts;

    if QUIET.load(Ordering::Relaxed) {
        return;
    }

    // Disable interrupts while holding the serial lock to prevent deadlock
    // if an interrupt handler also tries to print.
    interrupts::without_interrupts(|| {
        SERIAL1
            .lock()
            .write_fmt(args)
            .expect("printing to serial failed");
    });
}

/// Like `_print`, but ignores `QUIET` — for logging that must survive the
/// post-boot `set_quiet(true)` (written when the app signals boot-complete,
/// syscall.rs). Used by diagnostic instrumentation that needs to trace a running
/// node past boot, when routine logging is otherwise suppressed to keep the
/// serial-console eval shell clean. (See PAYDOWN: QUIET suppressing *all*
/// post-boot serial logging makes the console unusable for post-boot diagnostics.)
#[doc(hidden)]
pub fn _print_always(args: ::core::fmt::Arguments) {
    use core::fmt::Write;
    use x86_64::instructions::interrupts;
    interrupts::without_interrupts(|| {
        let _ = SERIAL1.lock().write_fmt(args);
    });
}

/// `serial_println!` that bypasses `QUIET` (see `_print_always`).
#[macro_export]
macro_rules! serial_println_always {
    () => { $crate::serial::_print_always(format_args!("\n")) };
    ($($arg:tt)*) => {
        $crate::serial::_print_always(format_args!("{}\n", format_args!($($arg)*)))
    };
}

/// Verbose/debug boot logging, **off by default**. Distinct from `QUIET` (which
/// silences routine logging only *after* boot): high-volume per-item boot spam
/// (`[vfs] open …` per beam file, `[accept]` per connection) fills the ~64 KB
/// EC2 console buffer *at* boot and drowns everything else, which broke
/// console-based post-boot diagnosis (BUG-8). Gate that spam behind `vdbg!` so
/// boot is quiet by default; flip `set_verbose(true)` to bring it back.
static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Enable (`true`) or disable (`false`) verbose/debug boot logging (`vdbg!`).
pub fn set_verbose(v: bool) {
    VERBOSE.store(v, Ordering::Relaxed);
}

/// Whether verbose/debug logging is enabled.
pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// `serial_println!` gated behind the verbose flag — quiet by default.
#[macro_export]
macro_rules! vdbg {
    ($($arg:tt)*) => {
        if $crate::serial::verbose() {
            $crate::serial_println!($($arg)*);
        }
    };
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
/// Uses the serial lock to prevent interleaving on SMP.
pub fn raw_str(s: &[u8]) {
    use x86_64::instructions::interrupts;
    interrupts::without_interrupts(|| {
        let _lock = SERIAL1.lock();
        for &b in s {
            // SAFETY: 0x3F8 is COM1 data port, 0x3FD is LSR.
            unsafe {
                while (x86_64::instructions::port::Port::<u8>::new(0x3FD).read() & 0x20) == 0 {}
                x86_64::instructions::port::Port::<u8>::new(0x3F8).write(b);
            }
        }
    });
}

/// Write directly to COM1 WITHOUT the serial lock.
/// Only for crash handlers where the lock might be held by another CPU.
/// Output may interleave with other serial output.
pub fn raw_str_nolock(s: &[u8]) {
    for &b in s {
        unsafe {
            while (x86_64::instructions::port::Port::<u8>::new(0x3FD).read() & 0x20) == 0 {}
            x86_64::instructions::port::Port::<u8>::new(0x3F8).write(b);
        }
    }
}

/// Write a hex u64 value to COM1 WITHOUT the serial lock (crash handler use only).
pub fn raw_hex_nolock(val: u64) {
    let hex = b"0123456789abcdef";
    raw_str_nolock(b"0x");
    let mut started = false;
    for i in (0..16).rev() {
        let nibble = ((val >> (i * 4)) & 0xf) as usize;
        if nibble != 0 || started || i == 0 {
            started = true;
            unsafe {
                while (x86_64::instructions::port::Port::<u8>::new(0x3FD).read() & 0x20) == 0 {}
                x86_64::instructions::port::Port::<u8>::new(0x3F8).write(hex[nibble]);
            }
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
