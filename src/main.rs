#![no_std]
#![no_main]

extern crate alloc;

mod boot;

use core::panic::PanicInfo;
use tyn_kernel::serial_println;
use virtio_drivers::transport::pci::bus::{Cam, Command, MmioCam, PciRoot};
use virtio_drivers::transport::pci::{PciTransport, virtio_device_type};
use virtio_drivers::transport::{DeviceType, Transport};

/// ECAM PCI config base for q35 (from QEMU's seabios dev-q35.h).
const MMCONFIG_BASE: usize = 0xB000_0000;

#[unsafe(no_mangle)]
extern "C" fn main(_mbi: *const u8) -> ! {
    serial_println!("=== Tyn Kernel v{} ===", env!("CARGO_PKG_VERSION"));

    tyn_kernel::memory::heap::init_static();
    tyn_kernel::drivers::virtio::hal::init_dma();
    tyn_kernel::interrupts::init_idt();

    // Clear CR0.TS (Task Switched) to allow SSE instructions in user code.
    // SAFETY: Clearing TS only affects FPU/SSE lazy state saving.
    unsafe {
        core::arch::asm!("clts", options(nomem, nostack));
    }

    // NOTE: CR4.TSD can't trap RDTSC in ring 0 (we run everything in ring 0).
    // The ERTS time-backwards issue from timer preemption needs a different fix.

    // Calibrate TSC frequency against PIT (before APIC takes over PIT)
    tyn_kernel::syscall::calibrate_tsc();

    // Discover CPUs via ACPI MADT and initialize APIC
    let acpi_info = tyn_kernel::acpi::discover_cpus();
    if let Some(ref info) = acpi_info {
        serial_println!("[boot] {} CPUs available", info.num_cpus);
        let ioapic_addr = info.ioapic.as_ref().map(|io| io.address);
        tyn_kernel::apic::init_bsp(info.local_apic_addr, ioapic_addr);
    }

    // Initialize SMP scheduler
    let ncpus = acpi_info.as_ref().map(|i| i.num_cpus).unwrap_or(1);
    tyn_kernel::sched::init(ncpus);

    // Boot Application Processors (if multi-CPU)
    // Disable interrupts during AP bringup to prevent heap allocator races
    if let Some(ref info) = acpi_info {
        x86_64::instructions::interrupts::disable();
        tyn_kernel::smp::boot_aps(info);
        x86_64::instructions::interrupts::enable();
    }

    // Initialize virtio-net via PCI enumeration
    init_networking();

    // Initialize in-memory VFS (cpio archive with OTP files)
    tyn_kernel::vfs::init();

    // Set up syscall entry point
    tyn_kernel::syscall::init();

    // Timer starts at first clone (sys_clone sets timer_active, calls init_timer).
    // Pre-clone init must run without interrupts — timer interferes with spin-waits.

    // Load and run embedded ELF binary
    // Use beam.smp for ERTS, hello.elf for testing
    static HELLO_ELF: &[u8] = include_bytes!("beam.smp.elf");
    serial_println!("[boot] ELF binary: {} bytes", HELLO_ELF.len());

    // The kernel's .rodata contains the embedded ELF and cpio archive.
    // With JIT, .rodata can be ~55 MB (34 MB ELF + 21 MB cpio). Kernel at
    // 240 MiB extends to ~295 MiB. Place copy buffers well above that.
    const ELF_COPY_BASE: usize = 0x1200_0000; // 288 MiB
    const CPIO_COPY_BASE: usize = ELF_COPY_BASE + 0x1A0_0000; // +26 MiB = 314 MiB
    // SAFETY: Destination regions are identity-mapped and above the kernel.
    let elf_copy = unsafe {
        let dst = ELF_COPY_BASE as *mut u8;
        core::ptr::copy_nonoverlapping(HELLO_ELF.as_ptr(), dst, HELLO_ELF.len());
        core::slice::from_raw_parts(dst, HELLO_ELF.len())
    };
    // Copy cpio to safe location and update the VFS to use it.
    unsafe {
        tyn_kernel::vfs::relocate(CPIO_COPY_BASE);
    }
    serial_println!("[boot] ELF copied to {:#x}, CPIO to {:#x}", ELF_COPY_BASE, CPIO_COPY_BASE);

    // SAFETY: Target addresses (0x400000+) are identity-mapped and writable.
    // Source data is at 32 MiB, safely above the load addresses.
    let info = unsafe { tyn_kernel::elf::load(elf_copy) }.expect("ELF load failed");
    serial_println!("[boot] ELF mem_end={:#x}", info.mem_end);

    // Set initial brk above the loaded ELF segments
    tyn_kernel::syscall::set_initial_brk(info.mem_end);

    // Allocate a user stack (2 MiB, within the 256M RAM region)
    const USER_STACK_BASE: u64 = 0x0E00_0000; // 224 MiB
    const USER_STACK_SIZE: u64 = 2 * 1024 * 1024;
    let user_stack_top = USER_STACK_BASE + USER_STACK_SIZE;
    serial_println!("[boot] zeroing stack at {:#x}..{:#x}", USER_STACK_BASE, user_stack_top);
    // SAFETY: Stack range is identity-mapped and unused.
    unsafe {
        core::ptr::write_bytes(USER_STACK_BASE as *mut u8, 0, USER_STACK_SIZE as usize);
    }
    serial_println!("[boot] stack zeroed");

    // Build initial stack for musl CRT.
    // musl _start expects: [rsp]=argc, [rsp+8..]=argv ptrs, NULL, envp ptrs, NULL, auxv
    let mut sp = user_stack_top;
    // SAFETY: Writing to identity-mapped stack memory.
    unsafe {
        // Put argv strings near top of stack
        let args: &[&[u8]] = &[
            b"/otp/erts-15.2.7/bin/beam.smp\0",
            b"-S\0", b"2:2\0",
            b"-A\0", b"1\0",
            b"--\0",
            b"-root\0", b"/otp\0",
            b"-bindir\0", b"/otp/erts-15.2.7/bin\0",
            b"-noshell\0",
            b"-noinput\0",
            b"-kernel\0", b"inet_backend\0", b"inet\0",
            // Manual ThousandIsland-style HTTP server. Bandit/ThousandIsland
            // itself stalls on Tyn (the GenServer/DynamicSupervisor handler
            // chain doesn't progress past accept) but every primitive it uses
            // — listen/accept/setopts({active,once})/controlling_process/
            // active-mode {tcp,S,Data} delivery/send/close — works in this
            // raw flow. Curl returns "Hi".
            b"-eval\0", b"erlang:display(starting), {ok,L}=gen_tcp:listen(8080,[binary,{reuseaddr,true}]), erlang:display(listening), {ok,S}=gen_tcp:accept(L), erlang:display(accepted), Self=self(), Pid=spawn(fun() -> erlang:display(h_start), receive {sock,Sk} -> erlang:display(h_got_sock), R1=inet:setopts(Sk,[{active,once}]), erlang:display({h_setopts,R1}), receive {tcp,Sk,D} -> erlang:display({h_got_tcp,byte_size(D)}), R2=gen_tcp:send(Sk,<<\"HTTP/1.0 200 OK\\r\\nContent-Length: 2\\r\\nConnection: close\\r\\n\\r\\nHi\">>), erlang:display({h_send,R2}), gen_tcp:close(Sk), Self ! done after 5000 -> Self ! h_timeout end end end), erlang:display(h_spawned), Rc=gen_tcp:controlling_process(S,Pid), erlang:display({ctrl,Rc}), Pid ! {sock,S}, erlang:display(parent_msg_sent), receive M -> erlang:display({parent_got,M}) after 12000 -> erlang:display(parent_timeout) end.\0",
        ];
        let mut arg_ptrs = [0u64; 20];
        for (i, arg) in args.iter().enumerate() {
            sp -= 2048; // must fit longest arg (diagnostic eval strings can be 1500+ bytes)
            core::ptr::copy_nonoverlapping(arg.as_ptr(), sp as *mut u8, arg.len());
            arg_ptrs[i] = sp;
        }
        let argc = args.len();

        // Put environment variables
        let envs: &[&[u8]] = &[
            b"ROOTDIR=/otp\0",
            b"BINDIR=/otp/erts-15.2.7/bin\0",
            b"EMU=beam\0",
            b"PROGNAME=beam.smp\0",
        ];
        let mut env_ptrs = [0u64; 8];
        for (i, env) in envs.iter().enumerate() {
            sp -= 256;
            core::ptr::copy_nonoverlapping(env.as_ptr(), sp as *mut u8, env.len());
            env_ptrs[i] = sp;
        }
        let envc = envs.len();

        // 16 bytes of pseudo-random data for AT_RANDOM (musl stack canary)
        sp -= 16;
        let at_random_ptr = sp;
        let mut tsc = core::arch::x86_64::_rdtsc();
        for i in 0..16u64 {
            *(sp.wrapping_add(i) as *mut u8) = tsc as u8;
            tsc = tsc.wrapping_mul(6364136223846793005).wrapping_add(1);
        }

        // Align to 16 bytes
        sp &= !0xF;

        // Build stack frame (grows down):
        // AT_NULL
        sp -= 16;
        *(sp as *mut u64) = 0;
        *((sp + 8) as *mut u64) = 0;

        // AT_RANDOM (25) — pointer to 16 random bytes
        sp -= 16;
        *(sp as *mut u64) = 25;
        *((sp + 8) as *mut u64) = at_random_ptr;

        // AT_ENTRY (9) — entry point of the program
        sp -= 16;
        *(sp as *mut u64) = 9;
        *((sp + 8) as *mut u64) = info.entry;

        // AT_PHNUM (5) — number of program headers
        sp -= 16;
        *(sp as *mut u64) = 5;
        *((sp + 8) as *mut u64) = info.phnum as u64;

        // AT_PHENT (4) — size of each program header entry
        sp -= 16;
        *(sp as *mut u64) = 4;
        *((sp + 8) as *mut u64) = info.phentsize as u64;

        // AT_PHDR (3) — address of program headers in memory
        sp -= 16;
        *(sp as *mut u64) = 3;
        *((sp + 8) as *mut u64) = info.phdr_vaddr;

        // AT_PAGESZ (6)
        sp -= 16;
        *(sp as *mut u64) = 6;
        *((sp + 8) as *mut u64) = 4096;

        // envp NULL terminator
        sp -= 8;
        *(sp as *mut u64) = 0;

        // envp pointers (in reverse order)
        for i in (0..envc).rev() {
            sp -= 8;
            *(sp as *mut u64) = env_ptrs[i];
        }

        // argv NULL terminator
        sp -= 8;
        *(sp as *mut u64) = 0;

        // argv pointers (in reverse order since stack grows down)
        for i in (0..argc).rev() {
            sp -= 8;
            *(sp as *mut u64) = arg_ptrs[i];
        }

        // argc
        sp -= 8;
        *(sp as *mut u64) = argc as u64;
    }

    serial_println!("[boot] launching ERTS at {:#x} sp={:#x}", info.entry, sp);
    tyn_kernel::syscall::jump_to_user(info.entry, sp);
}

/// Enumerate PCI bus and initialize virtio-net if found.
fn init_networking() {
    let mmconfig_base = MMCONFIG_BASE as *mut u8;
    // SAFETY: mmconfig_base is the ECAM MMIO region at 0xB0000000,
    // identity-mapped in our page tables.
    let mut root = PciRoot::new(unsafe { MmioCam::new(mmconfig_base, Cam::Ecam) });

    for (dev_fn, info) in root.enumerate_bus(0) {
        if let Some(vtype) = virtio_device_type(&info) {
            serial_println!("[pci] {}:{}.{} VirtIO {:?}",
                dev_fn.bus, dev_fn.device, dev_fn.function, vtype);
            root.set_command(
                dev_fn,
                Command::IO_SPACE | Command::MEMORY_SPACE | Command::BUS_MASTER,
            );

            let transport =
                PciTransport::new::<tyn_kernel::drivers::virtio::hal::TynHal, _>(&mut root, dev_fn)
                    .expect("PciTransport::new failed");

            if transport.device_type() == DeviceType::Network {
                tyn_kernel::net::init_with_transport(transport);
                return;
            }
        }
    }

    serial_println!("[net] no virtio-net device found, networking disabled");
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    serial_println!("KERNEL PANIC: {}", info);
    tyn_kernel::halt_loop();
}
