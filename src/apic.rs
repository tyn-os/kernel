//! Local APIC and I/O APIC driver.
//!
//! Replaces the 8259 PIC with APIC for interrupt delivery.
//! Each CPU has its own Local APIC with a timer for preemptive scheduling.
//! The I/O APIC routes external device interrupts (virtio-net).

use crate::serial_println;
use core::sync::atomic::{AtomicBool, Ordering};

/// Local APIC register offsets (from APIC base, memory-mapped)
const APIC_ID: u32 = 0x020;
const APIC_VERSION: u32 = 0x030;
const APIC_TPR: u32 = 0x080;       // Task Priority Register
const APIC_EOI: u32 = 0x0B0;       // End Of Interrupt
const APIC_SVR: u32 = 0x0F0;       // Spurious Interrupt Vector
const APIC_ICR_LOW: u32 = 0x300;   // Interrupt Command Register (low)
const APIC_ICR_HIGH: u32 = 0x310;  // Interrupt Command Register (high)
const APIC_LVT_TIMER: u32 = 0x320; // LVT Timer
const APIC_TIMER_INIT: u32 = 0x380; // Timer Initial Count
const APIC_TIMER_CURRENT: u32 = 0x390; // Timer Current Count
const APIC_TIMER_DIVIDE: u32 = 0x3E0; // Timer Divide Configuration

/// Timer interrupt vector
const TIMER_VECTOR: u8 = 32;
/// Spurious interrupt vector
const SPURIOUS_VECTOR: u8 = 0xFF;

/// APIC base address (from MADT, default 0xFEE00000)
static mut APIC_BASE: u64 = 0xFEE0_0000;
/// I/O APIC base address
static mut IOAPIC_BASE: u64 = 0xFEC0_0000;

static APIC_INITIALIZED: AtomicBool = AtomicBool::new(false);

// --- Local APIC register access ---

unsafe fn apic_read(reg: u32) -> u32 {
    let addr = (APIC_BASE + reg as u64) as *const u32;
    core::ptr::read_volatile(addr)
}

unsafe fn apic_write(reg: u32, val: u32) {
    let addr = (APIC_BASE + reg as u64) as *mut u32;
    core::ptr::write_volatile(addr, val);
}

// --- I/O APIC register access ---

unsafe fn ioapic_read(reg: u32) -> u32 {
    let base = IOAPIC_BASE as *mut u32;
    core::ptr::write_volatile(base, reg);               // IOREGSEL
    core::ptr::read_volatile(base.add(4))               // IOWIN (offset 0x10)
}

unsafe fn ioapic_write(reg: u32, val: u32) {
    let base = IOAPIC_BASE as *mut u32;
    core::ptr::write_volatile(base, reg);
    core::ptr::write_volatile(base.add(4), val);
}

/// Disable the legacy 8259 PIC by masking all IRQs.
fn disable_pic() {
    unsafe {
        // Remap PIC to vectors 0x20-0x2F (to avoid conflicts), then mask all
        x86_64::instructions::port::Port::<u8>::new(0x20).write(0x11); // ICW1
        x86_64::instructions::port::Port::<u8>::new(0xA0).write(0x11);
        x86_64::instructions::port::Port::<u8>::new(0x21).write(0x20); // ICW2: vectors 0x20-0x27
        x86_64::instructions::port::Port::<u8>::new(0xA1).write(0x28); // ICW2: vectors 0x28-0x2F
        x86_64::instructions::port::Port::<u8>::new(0x21).write(0x04); // ICW3
        x86_64::instructions::port::Port::<u8>::new(0xA1).write(0x02);
        x86_64::instructions::port::Port::<u8>::new(0x21).write(0x01); // ICW4
        x86_64::instructions::port::Port::<u8>::new(0xA1).write(0x01);
        // Mask all IRQs on both PICs
        x86_64::instructions::port::Port::<u8>::new(0x21).write(0xFF);
        x86_64::instructions::port::Port::<u8>::new(0xA1).write(0xFF);
    }
}

/// Initialize the BSP's Local APIC.
pub fn init_bsp(apic_addr: u32, ioapic_addr: Option<u32>) {
    unsafe {
        APIC_BASE = apic_addr as u64;
        if let Some(addr) = ioapic_addr {
            IOAPIC_BASE = addr as u64;
        }
    }

    // Disable legacy PIC — use APIC timers only
    disable_pic();

    unsafe {
        // Enable Local APIC via SVR (bit 8 = enable, vector = 0xFF)
        apic_write(APIC_SVR, (1 << 8) | SPURIOUS_VECTOR as u32);

        // Set Task Priority to 0 (accept all interrupts)
        apic_write(APIC_TPR, 0);

        // Set up APIC timer for preemptive scheduling
        // Divide by 16
        apic_write(APIC_TIMER_DIVIDE, 0x03); // divide config: 0011 = divide by 16

        // Calibrate: count how many ticks in ~10ms using PIT
        let ticks_per_10ms = calibrate_apic_timer();

        let id = apic_read(APIC_ID) >> 24;

        // Mark APIC initialized BEFORE starting the timer so the
        // ISR sends EOI to the APIC, not the disabled PIC.
        APIC_INITIALIZED.store(true, Ordering::Release);

        // Set timer_active for syscall exit sti
        extern "C" { static mut timer_active: u8; }
        timer_active = 1;

        // Save calibrated value for APs
        CALIBRATED_TICKS = ticks_per_10ms;

        // NOW start periodic timer
        apic_write(APIC_LVT_TIMER, (1 << 17) | TIMER_VECTOR as u32);
        apic_write(APIC_TIMER_INIT, ticks_per_10ms);

        serial_println!("[apic] BSP Local APIC ID={} enabled, timer=100Hz", id);

        serial_println!("[apic] APIC timer only (PIC disabled)");
    }

    x86_64::instructions::interrupts::enable();
}

/// Calibrated APIC timer value (set by BSP, used by APs).
static mut CALIBRATED_TICKS: u32 = 0;

/// Initialize an AP's Local APIC with timer.
pub fn init_ap() {
    unsafe {
        // Enable APIC via SVR
        apic_write(APIC_SVR, (1 << 8) | SPURIOUS_VECTOR as u32);
        // Accept all interrupts
        apic_write(APIC_TPR, 0);

        // Start periodic timer using BSP's calibrated value
        if CALIBRATED_TICKS > 0 {
            apic_write(APIC_TIMER_DIVIDE, 0x03);
            apic_write(APIC_LVT_TIMER, (1 << 17) | TIMER_VECTOR as u32);
            apic_write(APIC_TIMER_INIT, CALIBRATED_TICKS);
        }

        let id = apic_read(APIC_ID) >> 24;
        serial_println!("[apic] AP Local APIC ID={} enabled, timer=100Hz", id);
    }
}

/// Calibrate the APIC timer using the PIT as a reference.
/// Returns the number of APIC timer ticks in 10ms.
fn calibrate_apic_timer() -> u32 {
    unsafe {
        // Set APIC timer to one-shot, max count
        apic_write(APIC_LVT_TIMER, (1 << 16) | TIMER_VECTOR as u32); // masked one-shot
        apic_write(APIC_TIMER_INIT, 0xFFFF_FFFF);

        // Program PIT channel 2 for a 10ms delay
        // PIT frequency = 1193182 Hz. 10ms = 11932 ticks.
        let pit_count: u16 = 11932;
        // Channel 2, mode 0 (interrupt on terminal count), lobyte/hibyte
        x86_64::instructions::port::Port::<u8>::new(0x43).write(0xB0); // channel 2, mode 0
        x86_64::instructions::port::Port::<u8>::new(0x42).write((pit_count & 0xFF) as u8);
        x86_64::instructions::port::Port::<u8>::new(0x42).write((pit_count >> 8) as u8);

        // Gate PIT channel 2 (NMI_STATUS port 0x61)
        let val = x86_64::instructions::port::Port::<u8>::new(0x61).read();
        x86_64::instructions::port::Port::<u8>::new(0x61).write(val & 0xFC); // gate low
        x86_64::instructions::port::Port::<u8>::new(0x61).write(val | 0x01); // gate high

        // Wait for PIT channel 2 output to go high (bit 5 of port 0x61)
        while (x86_64::instructions::port::Port::<u8>::new(0x61).read() & 0x20) == 0 {
            core::hint::spin_loop();
        }

        // Read APIC timer current count
        let remaining = apic_read(APIC_TIMER_CURRENT);
        let elapsed = 0xFFFF_FFFFu32 - remaining;

        serial_println!("[apic] timer calibration: {} ticks/10ms", elapsed);
        elapsed
    }
}

/// Send End-Of-Interrupt to the Local APIC.
/// Must be called at the end of every interrupt handler.
pub fn eoi() {
    unsafe { apic_write(APIC_EOI, 0); }
}

/// Send an INIT IPI to the specified APIC ID.
pub fn send_init_ipi(apic_id: u8) {
    unsafe {
        // Set target in ICR high
        apic_write(APIC_ICR_HIGH, (apic_id as u32) << 24);
        // Send INIT: delivery mode 101 (INIT), level assert
        apic_write(APIC_ICR_LOW, 0x0000_4500);
        // Wait for delivery
        while apic_read(APIC_ICR_LOW) & (1 << 12) != 0 {
            core::hint::spin_loop();
        }
    }
}

/// Send a Startup IPI (SIPI) to the specified APIC ID.
/// `page` is the physical page number of the trampoline (e.g., 0x08 for address 0x8000).
pub fn send_sipi(apic_id: u8, page: u8) {
    unsafe {
        apic_write(APIC_ICR_HIGH, (apic_id as u32) << 24);
        // Send SIPI: delivery mode 110 (SIPI), vector = page number
        apic_write(APIC_ICR_LOW, 0x0000_4600 | page as u32);
        while apic_read(APIC_ICR_LOW) & (1 << 12) != 0 {
            core::hint::spin_loop();
        }
    }
}

/// Set up the I/O APIC to route virtio-net interrupt to CPU 0.
pub fn init_ioapic(irq: u8, vector: u8, dest_apic_id: u8) {
    // Each IRQ uses two 32-bit registers in the I/O APIC redirection table
    // Register 0x10 + 2*irq = low 32 bits, 0x11 + 2*irq = high 32 bits
    let reg_low = 0x10 + 2 * irq as u32;
    let reg_high = reg_low + 1;

    unsafe {
        // High: destination APIC ID in bits 24-31
        ioapic_write(reg_high, (dest_apic_id as u32) << 24);
        // Low: vector, delivery mode fixed (000), physical dest, active low, edge-triggered
        ioapic_write(reg_low, vector as u32); // unmasked, edge, fixed, physical
    }

    serial_println!("[ioapic] IRQ {} → vector {} → APIC ID {}", irq, vector, dest_apic_id);
}

/// Send a fixed IPI to wake an idle CPU. Uses vector 33 (one above timer).
pub fn send_ipi(target_apic_id: u8) {
    const IPI_VECTOR: u8 = 34;
    unsafe {
        apic_write(APIC_ICR_HIGH, (target_apic_id as u32) << 24);
        // Fixed delivery, level assert, edge triggered, vector IPI_VECTOR
        apic_write(APIC_ICR_LOW, (1 << 14) | IPI_VECTOR as u32);
        while apic_read(APIC_ICR_LOW) & (1 << 12) != 0 {
            core::hint::spin_loop();
        }
    }
}

/// Check if APIC is initialized.
pub fn is_initialized() -> bool {
    APIC_INITIALIZED.load(Ordering::Acquire)
}
