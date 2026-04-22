//! IDT with exception handlers, timer interrupt with IST.

use crate::serial_println;
use spin::Mutex;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::structures::gdt::{GlobalDescriptorTable, Descriptor};
use x86_64::VirtAddr;

/// Interrupt stack for the timer handler (avoids red zone corruption).
#[repr(align(16))]
struct AlignedStack([u8; 16384]);
static mut TIMER_IST_STACK: AlignedStack = AlignedStack([0; 16384]);

/// TSS with IST1 pointing to the timer interrupt stack.
static mut TSS: TaskStateSegment = TaskStateSegment::new();

/// GDT with kernel code segment + TSS descriptor.
static mut GDT: GlobalDescriptorTable = GlobalDescriptorTable::new();

static IDT: Mutex<InterruptDescriptorTable> = Mutex::new(InterruptDescriptorTable::new());

/// Set up GDT with TSS, then load IDT with IST-enabled timer handler.
pub fn init_idt() {
    // Set up TSS with IST1 = timer interrupt stack
    unsafe {
        let stack_top = &TIMER_IST_STACK.0 as *const u8 as u64 + 16384;
        TSS.interrupt_stack_table[0] = VirtAddr::new(stack_top); // IST1
    }

    // GDT layout must match boot: null(0x00), code32(0x08), code64(0x10), data(0x18).
    // Boot assembly set CS=0x10 (code64). We append TSS after.
    let tss_sel = unsafe {
        // 0x08: placeholder (boot had 32-bit code here)
        GDT.add_entry(Descriptor::kernel_code_segment());
        // 0x10: 64-bit kernel code (CS points here)
        GDT.add_entry(Descriptor::kernel_code_segment());
        // 0x18: kernel data
        GDT.add_entry(Descriptor::kernel_data_segment());
        // 0x20: TSS (takes 2 GDT slots)
        let ts = GDT.add_entry(Descriptor::tss_segment(&TSS));
        GDT.load_unsafe();
        ts
    };

    // Load the task register
    unsafe { x86_64::instructions::tables::load_tss(tss_sel); }

    // Set up IDT
    let mut idt = IDT.lock();
    idt.page_fault.set_handler_fn(page_fault_handler);
    idt.double_fault.set_handler_fn(double_fault_handler);
    idt.general_protection_fault.set_handler_fn(gpf_handler);
    idt.breakpoint.set_handler_fn(breakpoint_handler);
    idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
    idt.device_not_available.set_handler_fn(device_not_available_handler);
    idt.simd_floating_point.set_handler_fn(simd_handler);
    // Timer at vector 32 with IST1 — uses dedicated stack, avoids red zone
    unsafe {
        idt[32].set_handler_fn(timer_handler)
            .set_stack_index(0); // IST1 (0-indexed in API = IST entry 1)
    }
    spin::MutexGuard::leak(idt).load();
}

/// Update TSS.IST1 to the given stack top address. Called during context
/// switch so each thread gets its own timer interrupt stack.
pub fn set_ist1(stack_top: u64) {
    unsafe {
        TSS.interrupt_stack_table[0] = VirtAddr::new(stack_top);
    }
}

/// Initialize the PIT (Programmable Interval Timer) at ~100 Hz.
/// Also set up the PIC to deliver IRQ0 at vector 32.
pub fn init_timer() {
    unsafe {
        // Remap PIC: IRQ0-7 → vectors 32-39, IRQ8-15 → vectors 40-47
        // ICW1: start init, cascade mode, ICW4 needed
        x86_64::instructions::port::Port::<u8>::new(0x20).write(0x11);
        x86_64::instructions::port::Port::<u8>::new(0xA0).write(0x11);
        // ICW2: vector offsets
        x86_64::instructions::port::Port::<u8>::new(0x21).write(32);
        x86_64::instructions::port::Port::<u8>::new(0xA1).write(40);
        // ICW3: cascading
        x86_64::instructions::port::Port::<u8>::new(0x21).write(4);
        x86_64::instructions::port::Port::<u8>::new(0xA1).write(2);
        // ICW4: 8086 mode
        x86_64::instructions::port::Port::<u8>::new(0x21).write(0x01);
        x86_64::instructions::port::Port::<u8>::new(0xA1).write(0x01);
        // Mask all except IRQ0 (timer)
        x86_64::instructions::port::Port::<u8>::new(0x21).write(0xFE); // unmask IRQ0
        x86_64::instructions::port::Port::<u8>::new(0xA1).write(0xFF); // mask all slave

        // Program PIT channel 0 for ~100 Hz (divisor = 11932 = 0x2E9C)
        // Higher frequency gives ERTS more preemption slots for thread-progress.
        // The binary is patched to skip monotonic time backwards checks.
        x86_64::instructions::port::Port::<u8>::new(0x43).write(0x36);
        x86_64::instructions::port::Port::<u8>::new(0x40).write(0x9C); // low byte of 11932
        x86_64::instructions::port::Port::<u8>::new(0x40).write(0x2E); // high byte of 11932
    }
    // Clear any stale IRQs with EOI before enabling interrupts
    unsafe {
        x86_64::instructions::port::Port::<u8>::new(0x20).write(0x20);
        x86_64::instructions::port::Port::<u8>::new(0xA0).write(0x20);
    }
    // Set the timer_active flag so the syscall exit path knows to sti.
    unsafe {
        extern "C" { static mut timer_active: u8; }
        timer_active = 1;
    }
    // Enable interrupts
    x86_64::instructions::interrupts::enable();
}

extern "x86-interrupt" fn timer_handler(_frame: InterruptStackFrame) {
    // EOI first.
    unsafe {
        x86_64::instructions::port::Port::<u8>::new(0x20).write(0x20);
    }
    // Wake sleeping threads and preempt the current thread.
    // The ERTS binary is patched to skip the monotonic time backwards check.
    crate::thread::check_futex_waiters();
    crate::thread::yield_to_other();
}

extern "x86-interrupt" fn page_fault_handler(
    frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    let last_ret = unsafe {
        extern "C" { static last_syscall_ret: u64; }
        core::ptr::read_volatile(&last_syscall_ret)
    };
    let thread_idx = crate::thread::current_idx();
    serial_println!("#PF ip={:#x} cr2={:#x} err={:?} sc_ret={:#x} thread={}",
        frame.instruction_pointer, Cr2::read_raw(), error_code, last_ret, thread_idx);
    // Dump bytes at crash IP to see what instruction is there
    let ip = frame.instruction_pointer.as_u64() as *const u8;
    let bytes: [u8; 16] = unsafe {
        let mut b = [0u8; 16];
        for i in 0..16 { b[i] = *ip.add(i); }
        b
    };
    serial_println!("  code: {:02x?}", bytes);
    // Dump RSP area
    let rsp = frame.stack_pointer.as_u64() as *const u64;
    for i in 0..4 {
        let v = unsafe { *rsp.add(i) };
        serial_println!("  [rsp+{}] = {:#x}", i*8, v);
    }
    crate::halt_loop();
}

extern "x86-interrupt" fn double_fault_handler(
    _frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    crate::serial::raw_str(b"DOUBLE FAULT\n");
    crate::halt_loop();
}

extern "x86-interrupt" fn gpf_handler(frame: InterruptStackFrame, _error_code: u64) {
    serial_println!("#GP at {:#x}", frame.instruction_pointer);
    crate::halt_loop();
}

extern "x86-interrupt" fn breakpoint_handler(_frame: InterruptStackFrame) {}

extern "x86-interrupt" fn invalid_opcode_handler(frame: InterruptStackFrame) {
    serial_println!("#UD at {:#x} rsp={:#x} thread={}",
        frame.instruction_pointer, frame.stack_pointer, crate::thread::current_idx());
    crate::halt_loop();
}

extern "x86-interrupt" fn device_not_available_handler(_frame: InterruptStackFrame) {
    crate::serial::raw_str(b"#NM\n");
    crate::halt_loop();
}

extern "x86-interrupt" fn simd_handler(_frame: InterruptStackFrame) {
    crate::serial::raw_str(b"#XM\n");
    crate::halt_loop();
}
