//! Per-CPU state: GDT, TSS, kernel stack, IST stack.
//!
//! Each CPU gets its own GDT and TSS (required because TSS contains
//! per-CPU IST stack pointers). The IDT is shared across all CPUs.
//!
//! Following Hermit-OS: Box::leak fresh GDT/TSS for each CPU.

use alloc::boxed::Box;
use x86_64::structures::gdt::{GlobalDescriptorTable, Descriptor};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

use crate::serial_println;

/// Per-CPU data. Allocated on the heap and leaked (lives forever).
#[repr(C)]
pub struct PerCpuData {
    pub cpu_id: u32,
    pub apic_id: u32,
    pub kernel_stack: [u8; 16384],
    pub ist_stack: [u8; 16384],
}

const MAX_CPUS: usize = 16;

/// Array of per-CPU data pointers, indexed by cpu_id.
static mut PER_CPU: [*mut PerCpuData; MAX_CPUS] = [core::ptr::null_mut(); MAX_CPUS];

/// Saved GDT pointer and TSS selector per CPU (filled by alloc_cpu, used by load_cpu).
static mut GDT_PTRS: [*mut GlobalDescriptorTable; MAX_CPUS] = [core::ptr::null_mut(); MAX_CPUS];
static mut TSS_SELS: [u16; MAX_CPUS] = [0; MAX_CPUS];

/// Allocate per-CPU data, GDT, and TSS on the heap. Can be called from any CPU.
/// Does NOT load the GDT/TSS — call `load_cpu` on the target CPU for that.
pub fn alloc_cpu(cpu_id: u32, apic_id: u32) {
    let data = Box::leak(Box::new(PerCpuData {
        cpu_id,
        apic_id,
        kernel_stack: [0u8; 16384],
        ist_stack: [0u8; 16384],
    }));

    unsafe { PER_CPU[cpu_id as usize] = data as *mut PerCpuData; }

    let tss = Box::leak(Box::new(TaskStateSegment::new()));
    let ist_top = data.ist_stack.as_ptr() as u64 + 16384;
    tss.interrupt_stack_table[0] = VirtAddr::new(ist_top);

    let gdt = Box::leak(Box::new(GlobalDescriptorTable::new()));
    gdt.add_entry(Descriptor::kernel_code_segment());  // 0x08
    gdt.add_entry(Descriptor::kernel_code_segment());  // 0x10 (CS)
    gdt.add_entry(Descriptor::kernel_data_segment());  // 0x18
    let tss_sel = gdt.add_entry(Descriptor::tss_segment(tss)); // 0x20

    unsafe {
        GDT_PTRS[cpu_id as usize] = gdt as *mut GlobalDescriptorTable;
        TSS_SELS[cpu_id as usize] = tss_sel.0;
    }

    serial_println!("[percpu] CPU {} (APIC {}) allocated, IST1={:#x}",
        cpu_id, apic_id, ist_top);
}

/// Load the pre-allocated GDT and TSS on the CURRENT CPU.
/// Must be called on the target CPU (the CPU that will use this GDT/TSS).
pub fn load_cpu(cpu_id: u32) {
    unsafe {
        let gdt = &*GDT_PTRS[cpu_id as usize];
        gdt.load_unsafe();

        // Reload CS to 0x10 (code64 in our GDT layout).
        // lgdt only changes GDTR, not segment registers. The AP's CS
        // might be 0x08 from the trampoline GDT which has a different
        // layout. IDT entries reference CS=0x10, so we must match.
        core::arch::asm!(
            "push 0x10",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ax, 0x18",
            "mov ds, ax",
            "mov es, ax",
            "mov ss, ax",
            tmp = out(reg) _,
        );

        let sel = x86_64::structures::gdt::SegmentSelector(TSS_SELS[cpu_id as usize]);
        x86_64::instructions::tables::load_tss(sel);
    }
    crate::serial::raw_str(b"[percpu] GDT+TSS loaded\n");
}

/// Combined alloc + load for BSP (single call, same CPU).
pub fn init_cpu(cpu_id: u32, apic_id: u32) {
    alloc_cpu(cpu_id, apic_id);
    load_cpu(cpu_id);
}

/// Get the per-CPU data for a given CPU ID.
pub fn get(cpu_id: u32) -> Option<&'static PerCpuData> {
    unsafe {
        let ptr = PER_CPU[cpu_id as usize];
        if ptr.is_null() { None } else { Some(&*ptr) }
    }
}

/// Get the IST stack top for a given CPU (used by thread.rs for IST updates).
pub fn ist_stack_top(cpu_id: u32) -> u64 {
    unsafe {
        let ptr = PER_CPU[cpu_id as usize];
        if ptr.is_null() { 0 } else {
            (*ptr).ist_stack.as_ptr() as u64 + 16384
        }
    }
}
