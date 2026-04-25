//! SMP support — AP trampoline and bringup.
//!
//! The trampoline is copied to physical address 0x8000 before sending SIPI.
//! Each AP transitions: 16-bit real → 32-bit protected → 64-bit long → Rust.

use core::sync::atomic::{AtomicU32, Ordering};
use crate::serial_println;

/// Physical address where the trampoline is placed.
const TRAMPOLINE_ADDR: u64 = 0x8000;
/// SIPI vector = trampoline page number (0x8000 >> 12 = 0x08)
const SIPI_VECTOR: u8 = 0x08;

/// Parameter offsets within the trampoline page (match trampoline.S)
const OFF_ENTRY_POINT: usize = 0x08;
const OFF_CPU_ID: usize = 0x10;
const OFF_CR3: usize = 0x14;
const OFF_STACK_TOP: usize = 0x18;
const OFF_AP_READY: usize = 0x20;

/// Number of APs that have completed initialization.
static AP_STARTED_COUNT: AtomicU32 = AtomicU32::new(0);

/// The trampoline binary (assembled from trampoline.S).
/// For now, we write it as raw x86 machine code. This avoids needing
/// a separate assembly build step.
///
/// This is the minimal trampoline:
///   16-bit: cli, load GDT, enable PE, far jump to 32-bit
///   32-bit: set segments, enable PAE, load CR3, enable LME+NXE, enable PG, far jump to 64-bit
///   64-bit: set segments, load stack, set ready flag, call entry_point
static TRAMPOLINE: &[u8] = include_bytes!("trampoline.bin");

/// Boot all Application Processors discovered by ACPI.
pub fn boot_aps(acpi_info: &crate::acpi::AcpiInfo) {
    if acpi_info.num_cpus <= 1 {
        serial_println!("[smp] single CPU, no APs to boot");
        return;
    }

    // Get BSP's CR3 (page table base)
    let cr3: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3); }

    serial_println!("[smp] BSP CR3={:#x}, booting {} APs", cr3, acpi_info.num_cpus - 1);

    for i in 1..acpi_info.num_cpus {
        let cpu = &acpi_info.cpus[i];
        boot_ap(i as u32, cpu.apic_id, cr3);
    }

    serial_println!("[smp] all {} APs started", AP_STARTED_COUNT.load(Ordering::Acquire));
}

fn boot_ap(cpu_id: u32, apic_id: u8, cr3: u64) {
    // Pre-allocate per-CPU GDT+TSS on BSP (AP only needs to load, not allocate)
    crate::percpu::alloc_cpu(cpu_id, apic_id as u32);

    // Allocate a kernel stack for this AP (64 KiB, 16-byte aligned)
    let layout = alloc::alloc::Layout::from_size_align(65536, 16).unwrap();
    let stack_base = unsafe { alloc::alloc::alloc_zeroed(layout) };
    let stack_top = stack_base as u64 + 65536;

    // Copy trampoline to 0x8000
    unsafe {
        let dest = TRAMPOLINE_ADDR as *mut u8;
        core::ptr::copy_nonoverlapping(TRAMPOLINE.as_ptr(), dest, TRAMPOLINE.len());

        // Fill in parameters
        let base = TRAMPOLINE_ADDR as *mut u8;
        *(base.add(OFF_ENTRY_POINT) as *mut u64) = ap_main as u64;
        *(base.add(OFF_CPU_ID) as *mut u32) = cpu_id;
        *(base.add(OFF_CR3) as *mut u32) = cr3 as u32;
        *(base.add(OFF_STACK_TOP) as *mut u64) = stack_top;
        *(base.add(OFF_AP_READY) as *mut u32) = 0;
    }

    serial_println!("[smp] sending INIT+SIPI to APIC ID {} (cpu {})", apic_id, cpu_id);

    // INIT IPI
    crate::apic::send_init_ipi(apic_id);
    // Wait ~10ms (spin on TSC — ~20M cycles at 2GHz)
    spin_delay_us(10_000);

    // SIPI
    crate::apic::send_sipi(apic_id, SIPI_VECTOR);

    // Wait for AP to signal readiness (up to 1 second)
    let ready_ptr = (TRAMPOLINE_ADDR + OFF_AP_READY as u64) as *const u32;
    let mut waited = 0u32;
    while waited < 1_000_000 {
        unsafe {
            if core::ptr::read_volatile(ready_ptr) == 1 {
                serial_println!("[smp] CPU {} (APIC {}) reached 64-bit mode", cpu_id, apic_id);
                // Wait for AP to complete full init
                while AP_STARTED_COUNT.load(Ordering::Acquire) < cpu_id {
                    core::hint::spin_loop();
                }
                // Measure TSC offset between BSP and this AP
                crate::syscall::measure_tsc_offset(cpu_id as usize);
                return;
            }
        }
        spin_delay_us(10);
        waited += 10;
    }

    serial_println!("[smp] TIMEOUT waiting for CPU {} (APIC {})", cpu_id, apic_id);
}

/// AP entry point — called from trampoline after reaching 64-bit mode.
/// The AP has its own stack but no GDT/TSS/IDT yet.
extern "C" fn ap_main(cpu_id: u32) -> ! {
    // Get APIC ID from the Local APIC
    let apic_id = unsafe {
        let apic_base = 0xFEE0_0000u64;
        let id_reg = (apic_base + 0x20) as *const u32;
        (core::ptr::read_volatile(id_reg) >> 24) as u8
    };

    crate::serial::raw_str(b"[smp] AP in Rust\n");

    // Enable SSE on this CPU: clear CR0.TS and set CR4.OSFXSR + CR4.OSXMMEXCPT.
    // The trampoline only sets CR4.PAE. Without these, SSE instructions (#UD).
    unsafe {
        // Clear CR0.TS (Task Switched) — allows FPU/SSE without #NM
        core::arch::asm!("clts", options(nomem, nostack));
        // Set CR4.OSFXSR (bit 9) and CR4.OSXMMEXCPT (bit 10)
        let mut cr4: u64;
        core::arch::asm!("mov {}, cr4", out(reg) cr4);
        cr4 |= (1 << 9) | (1 << 10);
        core::arch::asm!("mov cr4, {}", in(reg) cr4);
    }

    // Load per-CPU GDT+TSS FIRST — the IDT entries reference CS=0x10
    // which must be a code segment. The trampoline's GDT64 has data at 0x10.
    crate::percpu::load_cpu(cpu_id);

    // Now load the IDT — CS=0x10 in the per-CPU GDT is code64, matching IDT entries
    crate::interrupts::load_idt();

    // Set up syscall MSRs on this CPU (LSTAR, STAR, SFMASK, EFER.SCE are per-CPU)
    crate::syscall::init_cpu_msrs(cpu_id as usize);

    // Enable this AP's Local APIC using the global APIC base
    crate::apic::init_ap();
    crate::serial::raw_str(b"[smp] AP APIC done\n");

    // Enable interrupts so we can receive IPIs
    x86_64::instructions::interrupts::enable();

    // Test IPI handler directly
    // IPI handler verified working (int 34 test passed)

    // Signal BSP that we're fully initialized
    AP_STARTED_COUNT.fetch_add(1, Ordering::Release);

    // TSC synchronization with BSP (after signaling ready)
    crate::syscall::ap_tsc_sync();

    crate::serial::raw_str(b"[smp] AP fully initialized\n");

    // Idle loop — wait for threads to be assigned via IPI
    loop {
        x86_64::instructions::hlt();
        // IPI or timer woke us — check queue (no serial output to avoid lock contention)
        crate::sched::yield_current();
    }
}

/// Spin-delay for approximately `us` microseconds using TSC.
fn spin_delay_us(us: u64) {
    let start = unsafe { core::arch::x86_64::_rdtsc() };
    // Assume ~2GHz TSC (2000 ticks/us). Conservative for delay purposes.
    let target = start + us * 2000;
    while unsafe { core::arch::x86_64::_rdtsc() } < target {
        core::hint::spin_loop();
    }
}
