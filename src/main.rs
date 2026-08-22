#![no_std]
#![no_main]

extern crate alloc;

mod boot;

use core::panic::PanicInfo;
use tyn_kernel::serial_println;
use virtio_drivers::transport::pci::bus::{Command, PciRoot};
use virtio_drivers::transport::pci::{PciTransport, virtio_device_type};
use virtio_drivers::transport::{DeviceType, Transport};

/// Read a little-endian u32 at `base + off`. Multiboot structures are
/// 4-byte aligned in practice, but `read_unaligned` is safe regardless.
///
/// # Safety
/// `base + off .. + 4` must be a readable, identity-mapped address.
#[inline]
unsafe fn mb_read_u32(base: *const u8, off: usize) -> u32 {
    unsafe { core::ptr::read_unaligned(base.add(off) as *const u32) }
}

/// Parse the GRUB multiboot1 module list, print where GRUB placed each module,
/// and return the `[mod_start, mod_end)` bounds of the first module that is a
/// valid newc cpio. Returns `None` when no module is present or the first
/// module is not a cpio (caller then falls back to the embedded cpio).
///
/// The module is NOT consumed here — the caller relocates it out of low RAM
/// (Phase 1b) before `elf::load`, because GRUB places it on top of the ERTS
/// load region.
///
/// Multiboot1 info layout: flags@0, mods_count@20, mods_addr@24. Bit 3 of
/// flags means the module fields are valid. Each module entry is 16 bytes:
/// { u32 mod_start; u32 mod_end; u32 cmdline; u32 pad; }.
fn parse_multiboot_module(mbi: *const u8) -> Option<(u32, u32)> {
    if mbi.is_null() {
        serial_println!("[mb] no multiboot info pointer (booted via QEMU -kernel?)");
        return None;
    }
    // SAFETY: GRUB places the multiboot info struct and module metadata in low
    // RAM, which is identity-mapped (boot page tables map the first 4 GiB). We
    // only ever read from these addresses.
    unsafe {
        let flags = mb_read_u32(mbi, 0);
        if flags & (1 << 3) == 0 {
            serial_println!("[mb] flags={:#x}: no modules present (bit 3 clear)", flags);
            return None;
        }
        let mods_count = mb_read_u32(mbi, 20);
        let mods_addr = mb_read_u32(mbi, 24);
        serial_println!("[mb] mods_count={}", mods_count);

        let embedded = tyn_kernel::vfs::embedded_len();
        let mut cpio_module: Option<(u32, u32)> = None;
        for i in 0..mods_count {
            let ent = (mods_addr as usize + i as usize * 16) as *const u8;
            let mod_start = mb_read_u32(ent, 0);
            let mod_end = mb_read_u32(ent, 4);
            let cmdline_ptr = mb_read_u32(ent, 8) as *const core::ffi::c_char;
            let size = mod_end.saturating_sub(mod_start);
            let cmdline = if cmdline_ptr.is_null() {
                ""
            } else {
                core::ffi::CStr::from_ptr(cmdline_ptr)
                    .to_str()
                    .unwrap_or("<non-utf8>")
            };
            serial_println!(
                "[mb] module {}: start={:#x} end={:#x} size={} cmdline=\"{}\"",
                i, mod_start, mod_end, size, cmdline
            );

            // A newc cpio archive begins with the ASCII magic "070701".
            let magic = core::slice::from_raw_parts(mod_start as usize as *const u8, 6);
            let is_cpio = magic == b"070701";
            let matches = size as usize == embedded;
            serial_println!(
                "[mb] module {} magic={} ({}), size {} embedded ({} bytes)",
                i,
                core::str::from_utf8(magic).unwrap_or("??????"),
                if is_cpio { "cpio newc OK" } else { "NOT cpio newc" },
                if matches { "matches" } else { "DIFFERS from" },
                embedded
            );

            // Use the first valid cpio module. A non-cpio module is not trusted
            // (caller falls back to embedded).
            if is_cpio && cpio_module.is_none() {
                cpio_module = Some((mod_start, mod_end));
            }
        }
        cpio_module
    }
}

#[unsafe(no_mangle)]
/// Read the optional per-image distribution cookie from the cpio (`tyn_cookie`,
/// written by `tyn-pack --cookie`). Its presence is what opts an image into
/// distributed boot — the committable, per-deployment replacement for the old
/// hardcoded `-setcookie` kernel constant. Returns the trimmed cookie, or None
/// if the file is absent or empty. Called after `vfs::init()`.
fn read_dist_cookie() -> Option<alloc::string::String> {
    if !tyn_kernel::vfs::exists(b"tyn_cookie") {
        return None;
    }
    let fd = tyn_kernel::vfs::open(b"tyn_cookie");
    if fd < 0 {
        return None;
    }
    let mut buf = [0u8; 256];
    // SAFETY: buf is a live, identity-mapped kernel stack buffer.
    let n = tyn_kernel::vfs::read(fd as i32, buf.as_mut_ptr(), buf.len());
    if n <= 0 {
        return None;
    }
    // Trim surrounding ASCII whitespace (a trailing newline from `printf`/`echo`).
    let raw = &buf[..n as usize];
    let start = raw.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(raw.len());
    let end = raw.iter().rposition(|b| !b.is_ascii_whitespace()).map_or(start, |i| i + 1);
    let trimmed = &raw[start..end];
    if trimmed.is_empty() {
        None
    } else {
        Some(alloc::string::String::from_utf8_lossy(trimmed).into_owned())
    }
}

extern "C" fn main(mbi: *const u8) -> ! {
    serial_println!("=== Tyn Kernel v{} ===", env!("CARGO_PKG_VERSION"));

    // Track 1 Phase 1b: parse the GRUB multiboot module list. If a cpio module
    // is present we relocate it to CPIO_HOME below (before elf::load); otherwise
    // the embedded cpio is used. Returns the module's low-memory bounds.
    let cpio_module = parse_multiboot_module(mbi);

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

    // Seed the wall clock from the RTC now that monotonic_ns() is meaningful.
    // CLOCK_REALTIME/gettimeofday serve real UTC after this; without it they'd
    // read 1970 + uptime. Monotonic time is unaffected.
    tyn_kernel::syscall::seed_wall_clock();

    // Upgrade CLOCK_REALTIME to kvmclock (host-exact scaling + drift correction,
    // ns resolution) when the hypervisor exposes it; the RTC seed above stays as
    // the fallback. Never worse than the RTC-seeded clock.
    tyn_kernel::pvclock::init();

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

    // Initialize NIC via PCI enumeration (virtio-net on QEMU, ENA on Nitro).
    // Use port-IO CF8/CFC for config access (portable across QEMU q35
    // and AWS Nitro). MCFG-discovery isn't required for Phase 1.
    init_networking();

    // Set up syscall entry point
    tyn_kernel::syscall::init();

    // Seed the kernel CSPRNG from the CPU hardware RNG. Panics if the CPU has no
    // RDRAND/RDSEED — we refuse to serve weak entropy to userspace crypto.
    tyn_kernel::rng::init();

    // Timer starts at first clone (sys_clone sets timer_active, calls init_timer).
    // Pre-clone init must run without interrupts — timer interferes with spin-waits.

    // Load and run embedded ELF binary
    // Use beam.smp for ERTS, hello.elf for testing
    static HELLO_ELF: &[u8] = include_bytes!("beam.smp.elf");
    serial_println!("[boot] ELF binary: {} bytes", HELLO_ELF.len());

    // The kernel's .rodata contains the embedded ELF and cpio archive.
    // Kernel at 240 MiB extends to ~291 MiB with current ELF (8.4 MB) +
    // cpio (~45 MB w/ Phoenix). Copy buffers must be above kernel's end
    // and below MMAP_NEXT base (now 0x1A00_0000 = 416 MiB).
    const ELF_COPY_BASE: usize = 0x1400_0000;            // 320 MiB
    // CPIO must sit above the ELF source buffer. Previously +10 MiB, but
    // the JIT-enabled beam.smp is ~10.1 MiB unstripped — it spilled into
    // the cpio region and clobbered the tail of the RW segment (including
    // the initialized `_dl_ns` pointer used by musl's __libc_setup_tls,
    // crashing JIT boots with #GP at 0x99cae4 / r14 = garbage). 16 MiB
    // gives headroom for any plausible BEAM build.
    const CPIO_COPY_BASE: usize = ELF_COPY_BASE + 0x180_0000; // +24 MiB = 344 MiB
    assert!(HELLO_ELF.len() <= CPIO_COPY_BASE - ELF_COPY_BASE,
        "embedded ELF would overlap CPIO buffer — bump CPIO_COPY_BASE");
    // SAFETY: Destination regions are identity-mapped and above the kernel.
    let elf_copy = unsafe {
        let dst = ELF_COPY_BASE as *mut u8;
        core::ptr::copy_nonoverlapping(HELLO_ELF.as_ptr(), dst, HELLO_ELF.len());
        core::slice::from_raw_parts(dst, HELLO_ELF.len())
    };
    // Choose the cpio source and place it at CPIO_HOME. Both the embedded copy
    // and a GRUB module end up at the same home, so the VFS (which reads from
    // CPIO_HOME) is source-agnostic.
    //
    // CPIO_HOME must stay below the mmap region (BeamAsm JIT pages) so a large
    // user app cpio can't silently overwrite it — hence the size ceiling.
    const CPIO_HOME: usize = CPIO_COPY_BASE;   // 0x1500_0000, 336 MiB
    const MMAP_BASE: usize = 0x1A00_0000;      // 416 MiB — start of JIT mmaps
    const CPIO_MAX: usize = MMAP_BASE - CPIO_HOME; // 0x0500_0000, ~80 MiB
    match cpio_module {
        Some((mod_start, mod_end)) => {
            let start = mod_start as usize;
            let end = mod_end as usize;
            let size = end - start;
            // Size ceiling: refuse to relocate a cpio that would spill into the
            // JIT mmap region. Panic loudly rather than silently corrupt.
            assert!(size <= CPIO_MAX,
                "cpio module too large: {} bytes, max {} (~80 MiB)", size, CPIO_MAX);
            // Source (module, low RAM) and dest (CPIO_HOME) must not overlap, or
            // copy_nonoverlapping would corrupt mid-copy. True for the known
            // addresses; assert so a future layout change fails loudly.
            assert!(end <= CPIO_HOME || start >= CPIO_HOME + size,
                "module [{:#x},{:#x}) overlaps CPIO_HOME [{:#x},{:#x})",
                start, end, CPIO_HOME, CPIO_HOME + size);
            serial_println!(
                "[vfs] module present, relocating {:#x} -> {:#x} ({} bytes), headroom {} bytes",
                start, CPIO_HOME, size, CPIO_MAX - size);
            // Copy the module up FIRST, then zero its low-memory staging area.
            // The staging range overlaps the ERTS load region + sbrk heap; 1a
            // proved leftover cpio bytes there #GP ERTS. Zeroing restores the
            // pristine "as if no module" state ERTS depends on. Never zero
            // before the copy.
            // SAFETY: both ranges are identity-mapped, non-overlapping, and the
            // staging range holds only the now-copied module (APs already up;
            // the AP trampoline at 0x8000 is below it).
            unsafe {
                tyn_kernel::vfs::relocate_from(start, size, CPIO_HOME);
                let zero_end = (end + 0xFFF) & !0xFFF; // page-align up
                core::ptr::write_bytes(start as *mut u8, 0, zero_end - start);
                serial_println!("[vfs] zeroed staging [{:#x},{:#x})", start, zero_end);
            }
        }
        None => {
            serial_println!("[vfs] no module, using embedded");
            // SAFETY: CPIO_HOME is identity-mapped and above the kernel.
            unsafe { tyn_kernel::vfs::relocate(CPIO_HOME); }
        }
    }
    serial_println!("[boot] ELF copied to {:#x}, CPIO to {:#x}", ELF_COPY_BASE, CPIO_HOME);

    // Initialize the VFS from the chosen source and prove which one is live.
    // (Module and embedded cpio are byte-identical in production, so only the
    // sentinel distinguishes them during the 1b provenance test.)
    tyn_kernel::vfs::init();
    // Mount the volatile in-memory tmpfs at /tmp and /dev/shm. The heap
    // allocator is already live (init_static above), which tmpfs requires.
    tyn_kernel::tmpfs::init();
    if tyn_kernel::vfs::exists(b"TYN_MODULE_SENTINEL") {
        serial_println!("[vfs] source=MODULE (sentinel present)");
    } else {
        serial_println!("[vfs] source=embedded (no sentinel)");
    }

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

    // Distributed boot is OPT-IN per image: only when the cpio carries a
    // `tyn_cookie` file (tyn-pack --cookie) do we resolve the node's own address
    // (drives DHCP to completion on ENA/Nitro; immediate on virtio's static IP)
    // and inject the dist flags below. No cookie → non-distributed, and we skip
    // the DHCP wait entirely.
    let dist_cookie = read_dist_cookie();
    let dist_name = if dist_cookie.is_some() {
        tyn_kernel::net::wait_for_dist_name(15_000)
    } else {
        None
    };

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
            // Raise ERTS process and port limits. Defaults can be as
            // low as 256 (+Q) in some minimal builds; with ~50 ports
            // used at boot this caps usable connections to ~200. Each
            // gen_tcp:accept allocates a port; Bandit spawns a process
            // per connection — both need headroom for sustained load.
            // (beam.smp accepts only `-`-prefixed flags directly; the
            // `+P` / `+Q` form is the erlexec convention. See
            // erts/emulator/beam/erl_init.c line ~1349 — the parser
            // calls erts_usage() and exits on any non-`-` argv[i].)
            // See directions/STRESS_TEST.md for the 200-wall finding.
            b"-P\0", b"65536\0",
            b"-Q\0", b"65536\0",
            b"--\0",
            b"-root\0", b"/otp\0",
            b"-bindir\0", b"/otp/erts-15.2.7/bin\0",
            b"-noshell\0",
            b"-noinput\0",
            b"-kernel\0", b"inet_backend\0", b"inet\0",
            // Distribution is OPT-IN per image: a node boots distributed iff the
            // cpio carries a `tyn_cookie` file (written by `tyn-pack --cookie`).
            // When present, the dist flags (-name n@<dhcp-ip>, -setcookie <it>,
            // -start_epmd false, -epmd_module tyn_epmd, fixed dist port 9100) are
            // injected after the args loop below — per-deployment config, no
            // hardcoded cookie in the shared kernel. Absent → non-distributed,
            // the unchanged default.
            // Bisection probe: does proc_lib:spawn work? does
            // gen_server:start_link work? Each stage prints before AND
            // after so we can see exactly which step stalls.
            // §B2.15: ThousandIsland with my_handler — a stripped-down
            // pure-Erlang gen_server that mimics TI.Handler's exact init
            // shape (Process.flag(:trap_exit, true), then waits for
            // {:thousand_island_ready, ...} in handle_info) but DOESN'T
            // use the `use ThousandIsland.Handler` macro.
            // §B2.16 probe: install a custom logger handler (crash_logger)
            // BEFORE starting ThousandIsland, so any crash in any
            // GenServer / supervisor chain prints to serial. TI's
            // Acceptor crashes silently after curl connects — we know
            // because the handler module is never even loaded — so this
            // catches whatever exception is being silently swallowed.
            // §B2.17 probe: replicate TI's listen options exactly, accept
            // the connection ourselves, and print exactly what gen_tcp
            // returns. This isolates whether the bug is in gen_tcp:accept
            // or in something TI does after.
            // §B2.18 probe: same TI options, but accept from inside a
            // Task (proc_lib-spawned, like TI's Acceptor) instead of the
            // main eval shell. If THIS fails but the main-shell version
            // passed, the bug is process-context dependent.
            // §B2.18 probe: cross-process accept. P1 (gen_server-like
            // process via Task) creates listen socket. P1 sends socket
            // to P2 (another Task) via message. P2 calls
            // gen_tcp:accept(L). This matches TI's Listener→Acceptor
            // socket ownership transfer.
            // §B2.19 probe: spawn 100 acceptor tasks all blocked on
            // gen_tcp:accept on the SAME listener socket. This matches
            // TI's default num_acceptors=100. When curl connects, only
            // one should wake — but if our kernel mishandles concurrent
            // accept-waiters, we'll see the failure mode here.
            // §B2.20 probe: bisect concurrent-accept-waiter count.
            // 1 worked. 100 didn't. Try 2.
            // Manual gen_tcp HTTP demo. Bandit/ThousandIsland themselves
            // stall on Tyn — see MESSAGE_DELIVERY.md §B2.16-§B2.20.
            // The bug isolated to concurrent gen_tcp:accept waiters: TI
            // spawns 100 acceptors by default and our kernel doesn't
            // deliver an incoming connection to any of them. With 1
            // waiter the kernel's accept-completion path works; with N
            // it doesn't. Likely fix is in src/net/socket.rs around
            // how inet_async accept replies are routed.
            // Every primitive (listen / accept / setopts({active,once}) /
            // controlling_process / active-mode {tcp,S,Data} delivery /
            // send / close) works in this raw flow. Curl returns "Hi".
            // §B2.21 verify-fix: same 100-acceptor stress test that
            // demonstrated the bug. After fixing sys_accept's race
            // (atomic check-and-swap inside with_net) and wiring up
            // fcntl(F_SETFL, O_NONBLOCK) for sockets, exactly one of
            // the 100 should wake on curl.
            // §B2.21 final verify: TI w/ ORIGINAL Connection.beam +
            // EchoHandler. With kernel-side accept race fixed, this
            // should now actually work end-to-end.
            // §B2.22: Bandit + HelloPlug (Elixir). With the kernel-side
            // sys_accept fix in place, Bandit's TI-based dispatch chain
            // should now work end-to-end. Both Bandit and HelloPlug were
            // compiled May 5 (before our kernel fix) but their bytecode
            // is unchanged — only the kernel's accept semantics changed.
            // Track 1 Phase 1c: the -eval is now app-agnostic. tyn_boot:start()
            // reads boot.config from the cpio, starts the shells + whatever app
            // the config names, and keeps printing the phoenix_listening marker.
            // Which app boots is decided by the cpio, not this kernel binary —
            // that's the "one kernel, many apps" claim Track 1 proves. The
            // embedded-cpio fallback ships no boot.config, so tyn_boot uses its
            // demo defaults (telemetry+jason+bench_plug on 8080), reproducing the
            // pre-1c boot exactly.
            b"-eval\0", b"tyn_boot:start().\0",
        ];
        let mut arg_ptrs = [0u64; 40];
        for (i, arg) in args.iter().enumerate() {
            sp -= 2048; // must fit longest arg (diagnostic eval strings can be 1500+ bytes)
            core::ptr::copy_nonoverlapping(arg.as_ptr(), sp as *mut u8, arg.len());
            arg_ptrs[i] = sp;
        }
        let mut argc = args.len();

        // OPT-IN distributed boot: iff the image carries a `tyn_cookie` file AND
        // we resolved an IPv4, inject the full dist flag set — a dynamic
        // `-name n@<dhcp-ip>` (the proven boot-arg path, made dynamic), the
        // per-image `-setcookie <tyn_cookie>` (replaces the old hardcoded spike
        // constant), and the EPMD-less setup (`-start_epmd false`,
        // `-epmd_module tyn_epmd`, fixed dist port 9100). Absent either → the node
        // boots non-distributed, the unchanged default.
        if let (Some(name), Some(cookie)) = (dist_name.as_ref(), dist_cookie.as_ref()) {
            let dist_args: [&[u8]; 14] = [
                b"-name", name.as_bytes(),
                b"-setcookie", cookie.as_bytes(),
                b"-start_epmd", b"false",
                b"-epmd_module", b"tyn_epmd",
                b"-kernel", b"inet_dist_listen_min", b"9100",
                b"-kernel", b"inet_dist_listen_max", b"9100",
            ];
            for a in dist_args.iter() {
                sp -= 2048;
                core::ptr::copy_nonoverlapping(a.as_ptr(), sp as *mut u8, a.len());
                *((sp + a.len() as u64) as *mut u8) = 0; // NUL-terminate
                arg_ptrs[argc] = sp;
                argc += 1;
            }
            crate::serial_println!("[dist] booting distributed as {} (cookie from tyn_cookie)", name);
        }

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

        // AT_BASE (7) — load bias of the interpreter (0 for static).
        sp -= 16;
        *(sp as *mut u64) = 7;
        *((sp + 8) as *mut u64) = 0;

        // AT_HWCAP (16) — hardware capability bitmask. musl on x86_64
        // detects features via CPUID directly so 0 is acceptable.
        sp -= 16;
        *(sp as *mut u64) = 16;
        *((sp + 8) as *mut u64) = 0;

        // AT_CLKTCK (17) — clock ticks per second.
        sp -= 16;
        *(sp as *mut u64) = 17;
        *((sp + 8) as *mut u64) = 100;

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

/// Enumerate PCI bus and initialize a NIC if found.
/// Prefers virtio-net (used on QEMU). Falls back to logging an ENA
/// device when running on AWS Nitro — Phase 1 of ENA support only
/// probes and reads version registers (see directions/ENA_DRIVER.md
/// for the full plan); Phase 2 wires it into smoltcp.
fn init_networking() {
    use virtio_drivers::transport::pci::bus::BarInfo;

    serial_println!("[pci] using port-IO config (CF8/CFC)");
    let mut root = PciRoot::new(tyn_kernel::net::pci_io::PortIoCam::new());

    // Walk all 256 PCI buses. Port-IO returns 0xFFFFFFFF for unmapped
    // slots (the standard sentinel), so we only see real devices —
    // no ghost devices from reading past an ECAM window.
    let mut devices: alloc::vec::Vec<_> = alloc::vec::Vec::new();
    let mut total = 0usize;
    let mut ghost = 0usize;
    for bus in 0u8..=255u8 {
        for (dev_fn, info) in root.enumerate_bus(bus) {
            total += 1;
            // Filter unmapped slots: 0xFFFF is the PCI standard, 0x0000
            // is what Nitro returns. Anything else we keep.
            if info.vendor_id == 0x0000 || info.vendor_id == 0xFFFF {
                continue;
            }
            // Real PCI devices we drive (virtio or ENA) live on bus 0
            // on every platform we target. Treat anything past bus 0
            // with an unrecognized vendor as a ghost from the bus-range
            // extending past the actual ECAM, and drop it silently to
            // keep the serial log readable on AWS Nitro.
            let recognised = virtio_device_type(&info).is_some()
                || tyn_kernel::net::ena::is_ena(info.vendor_id, info.device_id);
            if dev_fn.bus > 0 && !recognised {
                ghost += 1;
                continue;
            }
            devices.push((dev_fn, info));
        }
        if bus == 255 { break; }
    }
    serial_println!("[pci] scanned {} function slots, {} ghost, {} usable",
        total, ghost, devices.len());
    for (dev_fn, info) in &devices {
        serial_println!("[pci]   {:02x}:{:02x}.{} {:04x}:{:04x} class={:02x}.{:02x}",
            dev_fn.bus, dev_fn.device, dev_fn.function,
            info.vendor_id, info.device_id, info.class, info.subclass);
    }

    // First pass: virtio-net (QEMU / KVM dev path).
    for (dev_fn, info) in &devices {
        if let Some(vtype) = virtio_device_type(info) {
            serial_println!("[pci] {}:{}.{} VirtIO {:?}",
                dev_fn.bus, dev_fn.device, dev_fn.function, vtype);
            root.set_command(
                *dev_fn,
                Command::IO_SPACE | Command::MEMORY_SPACE | Command::BUS_MASTER,
            );

            let transport =
                PciTransport::new::<tyn_kernel::drivers::virtio::hal::TynHal, _>(&mut root, *dev_fn)
                    .expect("PciTransport::new failed");

            if transport.device_type() == DeviceType::Network {
                tyn_kernel::net::init_with_transport(transport);
                return;
            }
        }
    }

    // Second pass: AWS ENA (Nitro). Identified by vendor 0x1d0f.
    for (dev_fn, info) in &devices {
        if !tyn_kernel::net::ena::is_ena(info.vendor_id, info.device_id) {
            continue;
        }
        serial_println!(
            "[pci] {}:{}.{} ENA {:04x}:{:04x}",
            dev_fn.bus, dev_fn.device, dev_fn.function, info.vendor_id, info.device_id);
        root.set_command(
            *dev_fn,
            Command::MEMORY_SPACE | Command::BUS_MASTER,
        );
        let bars = root.bars(*dev_fn).expect("ENA bars()");
        let bar0 = match bars[0] {
            Some(BarInfo::Memory { address, .. }) => address,
            _ => {
                serial_println!("[ena] BAR0 is not a memory BAR; skipping");
                continue;
            }
        };
        tyn_kernel::net::ena::probe(
            bar0,
            info.device_id,
            (dev_fn.bus, dev_fn.device, dev_fn.function));
        // Phase 2B: admin queue + I/O queues + smoltcp (DHCP). On success the
        // global NetState is initialized and networking is live.
        if tyn_kernel::net::ena::init(bar0) {
            return;
        }
    }

    serial_println!("[net] no usable NIC found (virtio-net or fully-wired ENA), networking disabled");
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Always surface panics, even if post-boot quiet mode is on.
    tyn_kernel::serial::set_quiet(false);
    serial_println!("KERNEL PANIC: {}", info);
    tyn_kernel::halt_loop();
}
