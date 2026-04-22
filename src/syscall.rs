//! Linux syscall handler — traps `syscall` via LSTAR MSR.
//!
//! Assembly stub saves registers on a kernel stack, calls Rust dispatcher,
//! restores registers, returns via `sysretq`. Follows Kerla/rCore pattern.

use crate::serial_println;
use core::arch::global_asm;

/// Initialize the syscall entry point via MSRs.
pub fn init() {
    // SAFETY: Writing MSRs to configure syscall instruction handling.
    unsafe {
        use x86_64::registers::model_specific::Msr;

        // STAR: kernel CS/SS in bits 47:32. User segment not used (ring 0).
        let mut star = Msr::new(0xC000_0081);
        star.write(0x10u64 << 32);

        // LSTAR: syscall entry point.
        let mut lstar = Msr::new(0xC000_0082);
        lstar.write(syscall_entry as u64);

        // SFMASK: clear IF on syscall entry.
        let mut sfmask = Msr::new(0xC000_0084);
        sfmask.write(0x200);

        // Enable SCE (System Call Enable) in EFER.
        let mut efer = Msr::new(0xC000_0080);
        let val = efer.read();
        efer.write(val | 1);
    }
    // Initialize kernel stack pointer to thread 0's stack.
    unsafe {
        extern "C" { static mut current_kernel_stack: u64; }
        extern "C" { static syscall_stack_0_top: u8; }
        current_kernel_stack = &syscall_stack_0_top as *const u8 as u64;
    }
    // Mark main thread context as valid.
    crate::thread::init_main();
    serial_println!("[syscall] MSRs configured");
}

// ---------- Assembly entry stub ----------

global_asm!(
    // Per-thread kernel stacks (32 KiB each) + saved state.
    // Thread 0 = main, Thread 1 = child.
    ".section .bss",
    ".balign 4096",
    "syscall_stack_0_bottom: .space 32768",
    ".global syscall_stack_0_top",
    "syscall_stack_0_top:",
    "syscall_stack_1_bottom: .space 32768",
    ".global syscall_stack_1_top",
    "syscall_stack_1_top:",
    "saved_user_rsp: .quad 0",
    ".global last_syscall_ret",
    "last_syscall_ret: .quad 0",
    ".global saved_r9",
    "saved_r9: .quad 0",
    // Current kernel stack top pointer (switches between stacks 0 and 1)
    ".global current_kernel_stack",
    "current_kernel_stack: .quad 0",
    ".global timer_active",
    "timer_active: .byte 0",
    ".section .text",

    ".global syscall_entry",
    "syscall_entry:",
    // rcx = user return RIP (clobbered by syscall instruction)
    // r11 = user RFLAGS (clobbered by syscall instruction)
    // Use r11 as scratch since it's already clobbered by syscall.
    "mov r11, rsp",  // r11 = user RSP
    "mov rsp, [rip + current_kernel_stack]",
    // Push user RSP on kernel stack (per-thread, safe across yields)
    "push r11",      // user RSP
    "push 0",        // alignment padding (16 pushes total = even → RSP%16=8 after call)
    // Save ALL user registers that Linux syscall ABI preserves
    "push rcx",      // return RIP
    "push 0",        // placeholder for r11/RFLAGS (already clobbered)
    "push rdi",     // a0
    "push rsi",     // a1
    "push rdx",     // a2
    "push r8",      // a4
    "push r9",      // a5
    "push r10",     // a3
    "push rbx",
    "push rbp",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    // Save R9 for clone (musl passes fn in R9)
    "mov [rip + saved_r9], r9",
    // Shuffle: RAX=nr,RDI=a0,RSI=a1,RDX=a2,R10=a3,R8=a4,R9=a5
    //       → RDI=nr,RSI=a0,RDX=a1,RCX=a2,R8=a3,R9=a4
    "mov r9, r8",
    "mov r8, r10",
    "mov rcx, rdx",
    "mov rdx, rsi",
    "mov rsi, rdi",
    "mov rdi, rax",
    "call {dispatch}",
    // RAX now contains the return value
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbp",
    "pop rbx",
    "pop r10",
    "pop r9",
    "pop r8",
    "pop rdx",
    "pop rsi",
    "pop rdi",
    "pop r11",
    "pop rcx",
    "mov [rip + last_syscall_ret], rcx",
    "add rsp, 8",    // skip alignment padding
    // Restore kernel stack top for next syscall, then switch to user RSP
    "lea r11, [rsp + 8]",
    "mov [rip + current_kernel_stack], r11",
    "pop rsp",
    // Re-enable interrupts only after timer is active (first clone).
    // Pre-clone: IF stays 0 so spin-waits exhaust naturally.
    // Post-clone: IST protects red zone during timer preemption.
    "cmp byte ptr [rip + timer_active], 1",
    "jne 3f",
    "sti",
    "3:",
    // Validate return address is in ERTS code range
    "cmp rcx, 0x400000",
    "jb 2f",
    "cmp rcx, 0x900000",
    "ja 2f",
    "jmp rcx",
    "2:",
    // Bad return address — print and halt
    "push rcx",
    "push rax",
    "mov rdi, rcx",
    "call {bad_ret}",
    "pop rax",
    "pop rcx",
    "hlt",
    dispatch = sym syscall_dispatch,
    bad_ret = sym bad_return_address,
);

#[no_mangle]
extern "C" fn bad_return_address(addr: u64) {
    crate::serial::raw_str(b"BAD_RET@");
    crate::serial::raw_hex(addr);
    crate::serial::raw_str(b"\n");
}

extern "C" {
    fn syscall_entry();
}

// ---------- Syscall numbers (Linux x86_64) ----------

const SYS_READ: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_OPEN: u64 = 2;
const SYS_CLOSE: u64 = 3;
const SYS_LSEEK: u64 = 8;
const SYS_MMAP: u64 = 9;
const SYS_MPROTECT: u64 = 10;
const SYS_MUNMAP: u64 = 11;
const SYS_BRK: u64 = 12;
const SYS_RT_SIGACTION: u64 = 13;
const SYS_RT_SIGPROCMASK: u64 = 14;
const SYS_IOCTL: u64 = 16;
const SYS_ACCESS: u64 = 21;
const SYS_PIPE: u64 = 22;
const SYS_MADVISE: u64 = 28;
const SYS_GETPID: u64 = 39;
const SYS_UNAME: u64 = 63;
const SYS_FCNTL: u64 = 72;
const SYS_GETCWD: u64 = 79;
const SYS_READLINK: u64 = 89;
const SYS_GETUID: u64 = 102;
const SYS_GETGID: u64 = 104;
const SYS_GETEUID: u64 = 107;
const SYS_GETEGID: u64 = 108;
const SYS_ARCH_PRCTL: u64 = 158;
const SYS_GETDENTS64: u64 = 217;
const SYS_SET_TID_ADDRESS: u64 = 218;
const SYS_CLOCK_GETRES: u64 = 229;
const SYS_EXIT_GROUP: u64 = 231;
const SYS_EPOLL_CREATE1: u64 = 291;
const SYS_PIPE2: u64 = 293;
const SYS_PRLIMIT64: u64 = 302;
const SYS_SCHED_GETAFFINITY: u64 = 204;
const SYS_RSEQ: u64 = 334;
const SYS_TIMERFD_CREATE: u64 = 283;
const SYS_EPOLL_CTL: u64 = 233;
const SYS_NEWFSTATAT: u64 = 262;
const SYS_STAT: u64 = 4;
const SYS_FSTAT: u64 = 5;
const SYS_GETRANDOM: u64 = 318;
const SYS_OPENAT: u64 = 257;
const SYS_SET_ROBUST_LIST: u64 = 273;
const SYS_SIGALTSTACK: u64 = 131;
const SYS_PREAD64: u64 = 17;
const SYS_CLOCK_GETTIME: u64 = 228;
const SYS_SCHED_SETAFFINITY: u64 = 203;
const SYS_SOCKETPAIR: u64 = 53;
const SYS_FORK: u64 = 57;
const SYS_PPOLL: u64 = 271;
const SYS_TIMERFD_SETTIME: u64 = 286;
const SYS_PRCTL: u64 = 157;
const SYS_WRITEV: u64 = 20;
const SYS_TKILL: u64 = 200;
const SYS_FUTEX: u64 = 202;
const SYS_CLOCK_GETTIME64: u64 = 228; // actually clock_gettime uses 228 on x86_64
const SYS_EPOLL_WAIT: u64 = 232;
const SYS_EPOLL_PWAIT: u64 = 281;
const SYS_SELECT: u64 = 23;
const SYS_SCHED_YIELD: u64 = 24;
const SYS_NANOSLEEP: u64 = 35;
const SYS_CLONE: u64 = 56;
const SYS_EXIT: u64 = 60;
const SYS_TGKILL: u64 = 234;

// ---------- Mmap state ----------

use core::sync::atomic::{AtomicU64, Ordering};

/// Next available mmap address (bump allocator for anonymous mappings).
/// Must stay within the 4 GiB identity-mapped region and 256M RAM.
static MMAP_NEXT: AtomicU64 = AtomicU64::new(0x0800_0000); // Start at 128 MiB

/// brk heap top.
static BRK_TOP: AtomicU64 = AtomicU64::new(0);

// ---------- Dispatcher ----------

/// Per-thread flag: true when a thread is inside the syscall dispatcher.
/// Timer interrupt preempts ONLY threads in syscalls (futex spin-waits).
/// User code is never preempted — ERTS uses inline RDTSC which breaks
/// if preempted between two reads.
static IN_SYSCALL: [core::sync::atomic::AtomicBool; 24] = {
    const F: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    [F; 24]
};

/// Check if the CURRENT thread is inside a syscall handler.
pub fn in_syscall() -> bool {
    let idx = crate::thread::current_idx();
    if idx < 24 { IN_SYSCALL[idx].load(Ordering::Relaxed) } else { false }
}

/// Rust syscall dispatcher — called from assembly stub.
#[no_mangle]
extern "C" fn syscall_dispatch(
    nr: u64,
    a0: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    _a4: u64,
) -> i64 {
    {
        static SC: AtomicU64 = AtomicU64::new(0);
        let c = SC.fetch_add(1, Ordering::Relaxed);
        // Logging disabled for clean output
        let _ = (c, nr, a0);
    }
    let idx = crate::thread::current_idx();
    if idx < 24 { IN_SYSCALL[idx].store(true, Ordering::Relaxed); }
    let result = syscall_dispatch_inner(nr, a0, a1, a2, a3, _a4);
    if idx < 24 { IN_SYSCALL[idx].store(false, Ordering::Relaxed); }
    result
}

#[inline(always)]
fn syscall_dispatch_inner(
    nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, _a4: u64,
) -> i64 {
    match nr {
        SYS_WRITE => sys_write(a0 as i32, a1 as *const u8, a2 as usize),
        SYS_READ => sys_read(a0 as i32, a1 as *mut u8, a2 as usize),
        SYS_EXIT_GROUP => sys_exit_group(a0 as i32),
        SYS_BRK => sys_brk(a0),
        SYS_MMAP => sys_mmap(a0, a1, a2 as i32),
        SYS_MUNMAP => 0, // no-op
        SYS_MPROTECT => 0, // no-op
        SYS_MADVISE => 0, // no-op
        SYS_ARCH_PRCTL => sys_arch_prctl(a0 as i32, a1),
        SYS_SET_TID_ADDRESS => 1, // return "pid" 1
        SYS_SET_ROBUST_LIST => 0,
        SYS_RSEQ => -38, // -ENOSYS (not needed)
        SYS_GETPID => 1,
        SYS_GETUID | SYS_GETEUID => 0,
        SYS_GETGID | SYS_GETEGID => 0,
        SYS_UNAME => sys_uname(a0 as *mut u8),
        SYS_SCHED_GETAFFINITY => sys_sched_getaffinity(a1 as usize, a2 as *mut u8),
        SYS_CLOCK_GETRES => sys_clock_getres(a1 as *mut u64),
        SYS_PRLIMIT64 => sys_prlimit64(a2 as *const u8, a3 as *mut u8),
        SYS_OPEN | SYS_OPENAT => sys_open(a0, a1, nr),
        SYS_CLOSE => {
            crate::vfs::close(a0 as i32);
            crate::pipe::close(a0 as i32);
            0
        }
        SYS_STAT => sys_stat(a0 as *const u8, a1 as *mut u8),
        SYS_FSTAT => sys_fstat(a0 as i32, a1 as *mut u8),
        SYS_NEWFSTATAT => sys_fstat(a0 as i32, a2 as *mut u8),
        SYS_LSEEK => {
            if crate::vfs::is_vfs_fd(a0 as i32) {
                crate::vfs::lseek(a0 as i32, a1 as i64, a2 as i32)
            } else {
                0
            }
        }
        SYS_FCNTL => sys_fcntl(a0 as i32, a1 as i32, a2),
        SYS_ACCESS => -2, // -ENOENT
        SYS_READLINK => sys_readlink(a0 as *const u8, a1 as *mut u8, a2 as usize),
        SYS_GETDENTS64 => sys_getdents64(a0 as i32, a1 as *mut u8, a2 as usize),
        SYS_PIPE | SYS_PIPE2 => sys_pipe(a0 as *mut i32),
        SYS_EPOLL_CREATE1 => 50, // fake epoll fd
        SYS_TIMERFD_CREATE => 51, // fake timerfd
        SYS_EPOLL_CTL => 0,
        SYS_EPOLL_WAIT | SYS_EPOLL_PWAIT => sys_epoll_wait(a0, a1, a2),
        SYS_RT_SIGACTION => 0, // record but no-op
        SYS_RT_SIGPROCMASK => 0,
        SYS_SIGALTSTACK => 0,
        SYS_IOCTL => -25, // -ENOTTY
        SYS_PREAD64 => {
            if crate::vfs::is_vfs_fd(a0 as i32) {
                // pread64(fd, buf, count, offset)
                crate::vfs::lseek(a0 as i32, a3 as i64, 0); // SEEK_SET
                crate::vfs::read(a0 as i32, a1 as *mut u8, a2 as usize)
            } else {
                0
            }
        }
        SYS_CLOCK_GETTIME => sys_clock_gettime(a0 as i32, a1 as *mut u64),
        SYS_WRITEV => sys_writev(a0 as i32, a1 as *const IoVec, a2 as usize),
        SYS_GETRANDOM => sys_getrandom(a0 as *mut u8, a1 as usize),
        SYS_GETCWD => sys_getcwd(a0 as *mut u8, a1 as usize),
        SYS_TKILL | SYS_TGKILL => 0,
        SYS_SCHED_SETAFFINITY => 0,
        SYS_SOCKETPAIR => sys_pipe(a3 as *mut i32), // fake as pipe pair
        SYS_PRCTL => 0, // no-op
        SYS_FUTEX => sys_futex(a0, a1, a2),
        SYS_PPOLL => sys_ppoll(a0, a1),
        SYS_SELECT => {
            crate::thread::yield_to_other();
            0 // timeout expired
        }
        SYS_TIMERFD_SETTIME => 0,
        SYS_SCHED_YIELD => {
            crate::thread::yield_to_other();
            0
        }
        SYS_NANOSLEEP => { crate::thread::yield_to_other(); 0 }
        SYS_FORK => -38, // -ENOSYS
        SYS_CLONE => {
            // Allow clone but log. With our single-scheduler ERTS patch,
            // only 2 auxiliary threads are created (signal handler + poll).
            sys_clone(a0, a1, a2, a3, _a4)
        }
        SYS_EXIT => sys_exit_group(a0 as i32),
        41 | 42 | 43 | 44 | 45 | 46 | 47 | 48 | 49 | 50 | 51 | 54 | 55 => {
            // Socket syscalls: socket, connect, accept, sendto, recvfrom,
            // sendmsg, recvmsg, shutdown, bind, listen, getsockname,
            // setsockopt, getsockopt → not supported
            -97 // -EAFNOSUPPORT
        }
        _ => {
            serial_println!("[syscall] UNHANDLED nr={}", nr);
            -38 // -ENOSYS
        }
    }
}

// ---------- Implementations ----------

fn sys_write(fd: i32, buf: *const u8, count: usize) -> i64 {
    if fd == 1 || fd == 2 {
        for i in 0..count {
            // SAFETY: buf is in identity-mapped user memory.
            let byte = unsafe { *buf.add(i) };
            // SAFETY: 0x3F8 is COM1 data port, 0x3FD is LSR.
            unsafe {
                while (x86_64::instructions::port::Port::<u8>::new(0x3FD).read() & 0x20) == 0 {}
                x86_64::instructions::port::Port::<u8>::new(0x3F8).write(byte);
            }
        }
        count as i64
    } else if fd as i64 == FD_DEVNULL {
        count as i64 // silently discard
    } else if crate::pipe::is_pipe_fd(fd) {
        let result = crate::pipe::write(fd, buf, count);
        // Log first pipe write
        {
            static LOGGED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
            if !LOGGED.swap(true, Ordering::Relaxed) {
                serial_println!("[pipe] write fd={} count={} result={}", fd, count, result);
            }
        }
        result
    } else {
        -9 // -EBADF
    }
}

fn sys_read(fd: i32, buf: *mut u8, count: usize) -> i64 {
    // stdin: read from COM1 serial port
    if fd == 0 {
        return sys_read_stdin(buf, count);
    }
    if crate::vfs::is_vfs_fd(fd) {
        return crate::vfs::read(fd, buf, count);
    }
    if crate::pipe::is_pipe_fd(fd) {
        return crate::pipe::read(fd, buf, count);
    }
    // timerfd read: return expiration count (1).
    if fd == 51 {
        crate::thread::yield_to_other();
        if count >= 8 {
            unsafe { *(buf as *mut u64) = 1; }
        }
        return 8;
    }
    0 // EOF for other fds
}

/// Read from COM1 serial port (stdin). Blocks until at least one byte is available.
fn sys_read_stdin(buf: *mut u8, count: usize) -> i64 {
    if count == 0 { return 0; }

    // Poll COM1 LSR (0x3FD) bit 0 for data ready.
    // Yield between polls to avoid busy-waiting.
    loop {
        let lsr = unsafe { x86_64::instructions::port::Port::<u8>::new(0x3FD).read() };
        if lsr & 1 != 0 {
            // Data available — read one byte
            let byte = unsafe { x86_64::instructions::port::Port::<u8>::new(0x3F8).read() };
            unsafe { *buf = byte; }
            return 1;
        }
        // No data — yield and retry (non-blocking poll)
        crate::thread::yield_to_other();
    }
}

fn sys_exit_group(status: i32) -> i64 {
    serial_println!("[syscall] exit_group({})", status);
    crate::halt_loop();
}

fn sys_brk(addr: u64) -> i64 {
    if addr == 0 {
        // Query current brk
        let current = BRK_TOP.load(Ordering::Relaxed);
        if current == 0 {
            // First call — set initial brk after the binary
            BRK_TOP.store(0x1000000, Ordering::Relaxed); // 16 MiB
            0x1000000
        } else {
            current as i64
        }
    } else {
        // Set new brk
        BRK_TOP.store(addr, Ordering::Relaxed);
        addr as i64
    }
}

fn sys_mmap(addr: u64, length: u64, _prot: i32) -> i64 {
    let aligned = (length + 0xFFF) & !0xFFF; // page-align
    if addr != 0 {
        // Fixed address — zero it
        // SAFETY: addr is identity-mapped within our 4 GiB region.
        if aligned <= 0x400_0000 { // up to 64 MiB
            unsafe { core::ptr::write_bytes(addr as *mut u8, 0, aligned as usize) };
        }
        addr as i64
    } else {
        // Allocate from bump allocator and zero
        let result = MMAP_NEXT.fetch_add(aligned, Ordering::Relaxed);
        // SAFETY: result is identity-mapped within RAM.
        // QEMU zeros all RAM on boot, so only re-zero allocations that
        // might overlap previously used memory. Skip huge allocations
        // (ERTS's 1 GiB block) — QEMU already zeroed them.
        if aligned <= 0x400_0000 { // up to 64 MiB
            unsafe { core::ptr::write_bytes(result as *mut u8, 0, aligned as usize) };
        }
        result as i64
    }
}

fn sys_arch_prctl(code: i32, addr: u64) -> i64 {
    const ARCH_SET_FS: i32 = 0x1002;
    if code == ARCH_SET_FS {
        // SAFETY: Writing FS_BASE MSR for TLS.
        unsafe {
            x86_64::registers::model_specific::Msr::new(0xC000_0100).write(addr);
        }
        0
    } else {
        -22 // -EINVAL
    }
}

fn sys_uname(buf: *mut u8) -> i64 {
    // struct utsname: 5 fields × 65 bytes each
    // SAFETY: buf points to user memory (identity-mapped).
    unsafe {
        core::ptr::write_bytes(buf, 0, 65 * 5);
        let fields = [b"Linux" as &[u8], b"tyn", b"6.1.0-tyn", b"Tyn Kernel", b"x86_64"];
        for (i, field) in fields.iter().enumerate() {
            core::ptr::copy_nonoverlapping(
                field.as_ptr(),
                buf.add(i * 65),
                field.len(),
            );
        }
    }
    0
}

fn sys_sched_getaffinity(len: usize, mask: *mut u8) -> i64 {
    // Report 1 CPU (bit 0 set)
    if len >= 8 {
        // SAFETY: mask points to user memory.
        unsafe {
            core::ptr::write_bytes(mask, 0, len);
            *mask = 1; // CPU 0
        }
        8 // return size of mask written
    } else {
        -22 // -EINVAL
    }
}

fn sys_clock_getres(res: *mut u64) -> i64 {
    if !res.is_null() {
        // SAFETY: res points to user memory (struct timespec = tv_sec + tv_nsec).
        unsafe {
            *res = 0;           // tv_sec
            *(res.add(1)) = 1;  // tv_nsec = 1ns resolution
        }
    }
    0
}

fn sys_prlimit64(_new: *const u8, old: *mut u8) -> i64 {
    if !old.is_null() {
        // SAFETY: old points to user struct rlimit64 (16 bytes).
        unsafe {
            let p = old as *mut u64;
            *p = 8 * 1024 * 1024;       // rlim_cur = 8 MiB
            *(p.add(1)) = u64::MAX;     // rlim_max = unlimited
        }
    }
    0
}

/// Fake fd for /dev/null.
const FD_DEVNULL: i64 = 100;

fn sys_open(a0: u64, a1: u64, nr: u64) -> i64 {
    // For SYS_OPENAT, the filename is in a1 (a0 is dirfd).
    // For SYS_OPEN, the filename is in a0.
    let path_ptr = if nr == SYS_OPENAT { a1 } else { a0 } as *const u8;

    // Read path as a byte slice (up to 256 bytes).
    let mut path_buf = [0u8; 256];
    let mut path_len = 0;
    // SAFETY: path_ptr is in identity-mapped user memory.
    unsafe {
        while path_len < 255 {
            let b = *path_ptr.add(path_len);
            if b == 0 { break; }
            path_buf[path_len] = b;
            path_len += 1;
        }
    }
    let path = &path_buf[..path_len];

    // Check /dev/null
    if path == b"/dev/null" {
        return FD_DEVNULL;
    }

    // Try the VFS (cpio archive)
    let vfs_fd = crate::vfs::open(path);
    if vfs_fd >= 0 {
        return vfs_fd;
    }

    // If no VFS file found, check if it's a directory we can list
    if crate::vfs::is_dir_prefix(path) {
        // Return a directory fd. Use fd 900+N for directory fds.
        static DIR_FD_NEXT: core::sync::atomic::AtomicI32 =
            core::sync::atomic::AtomicI32::new(900);
        let dfd = DIR_FD_NEXT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        crate::vfs::open_dir(dfd, path);
        return dfd as i64;
    }

    -2 // -ENOENT
}

fn sys_getdents64(fd: i32, buf: *mut u8, count: usize) -> i64 {
    crate::vfs::getdents64(fd, buf, count)
}

fn sys_stat(path_ptr: *const u8, buf: *mut u8) -> i64 {
    // Read path string
    let mut path_buf = [0u8; 256];
    let mut path_len = 0;
    unsafe {
        while path_len < 255 {
            let b = *path_ptr.add(path_len);
            if b == 0 { break; }
            path_buf[path_len] = b;
            path_len += 1;
        }
    }
    let path = &path_buf[..path_len];

    // Check if it's a VFS path (directory or file)
    if !buf.is_null() {
        unsafe { core::ptr::write_bytes(buf, 0, 144); }
    }

    // Try opening as a file to get size
    let vfs_fd = crate::vfs::open(path);
    if vfs_fd >= 0 {
        if !buf.is_null() {
            unsafe {
                let mode_ptr = buf.add(24) as *mut u32;
                *mode_ptr = 0o100644; // regular file
                if let Some(size) = crate::vfs::fstat_size(vfs_fd as i32) {
                    let size_ptr = buf.add(48) as *mut u64;
                    *size_ptr = size as u64;
                }
                let blksize_ptr = buf.add(56) as *mut u64;
                *blksize_ptr = 4096;
            }
        }
        crate::vfs::close(vfs_fd as i32);
        return 0;
    }

    // Check if it's a known directory prefix
    if path.starts_with(b"/otp") {
        if !buf.is_null() {
            unsafe {
                let mode_ptr = buf.add(24) as *mut u32;
                *mode_ptr = 0o40755; // directory
            }
        }
        return 0;
    }

    -2 // -ENOENT
}

fn sys_fstat(fd: i32, buf: *mut u8) -> i64 {
    if !buf.is_null() {
        // SAFETY: buf points to user struct stat (144 bytes on x86_64).
        unsafe {
            core::ptr::write_bytes(buf, 0, 144);
            // Set st_mode to regular file (S_IFREG | 0644)
            let mode_ptr = buf.add(24) as *mut u32; // st_mode at offset 24
            *mode_ptr = 0o100644;
            // Set st_size if this is a VFS file
            if let Some(size) = crate::vfs::fstat_size(fd) {
                let size_ptr = buf.add(48) as *mut u64; // st_size at offset 48
                *size_ptr = size as u64;
            }
            // Set st_blksize
            let blksize_ptr = buf.add(56) as *mut u64; // st_blksize at offset 56
            *blksize_ptr = 4096;
        }
    }
    0
}

fn sys_getrandom(buf: *mut u8, len: usize) -> i64 {
    // SAFETY: buf points to user memory; RDTSC is always available.
    unsafe {
        let mut tsc = core::arch::x86_64::_rdtsc();
        for i in 0..len {
            *buf.add(i) = tsc as u8;
            tsc = tsc.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
    }
    len as i64
}

fn sys_pipe(fds: *mut i32) -> i64 {
    if fds.is_null() {
        return -22; // -EINVAL
    }
    let (read_fd, write_fd) = crate::pipe::create();
    // SAFETY: fds points to user memory (two consecutive i32s).
    unsafe {
        *fds = read_fd;
        *fds.add(1) = write_fd;
    }
    0
}

static CLONE_COUNT: AtomicU64 = AtomicU64::new(0);

fn sys_clone(flags: u64, stack: u64, parent_tid: u64, child_tid: u64, tls: u64) -> i64 {
    if stack == 0 {
        return -22; // -EINVAL
    }

    let clone_num = CLONE_COUNT.fetch_add(1, Ordering::Relaxed);
    let tid = (clone_num + 2) as i32;

    // Start preemptive timer on first clone. IST protects the red zone.
    if clone_num == 0 {
        crate::interrupts::init_timer();
    }

    // musl's __clone puts arg at [stack] and passes fn through R9.
    // SAFETY: stack points to identity-mapped user memory; saved_r9 is a BSS global.
    let fn_ptr = unsafe {
        extern "C" { static saved_r9: u64; }
        core::ptr::read_volatile(&saved_r9)
    };

    // CLONE_PARENT_SETTID: write TID to parent_tid (ptid).
    // Do NOT write to child_tid (ctid) — musl passes &__thread_list_lock
    // as ctid for CLONE_CHILD_CLEARTID (kernel clears it on thread exit).
    if (flags & 0x00100000) != 0 && parent_tid != 0 {
        // SAFETY: parent_tid points to user memory.
        unsafe { *(parent_tid as *mut u32) = tid as u32; }
    }

    serial_println!("[clone] #{} tid={} ptid={:#x} ctid={:#x}", clone_num, tid, parent_tid, child_tid);

    // Create cooperative threads for all clones.
    // SAFETY: fn_ptr and stack are from musl's pthread_create.
    unsafe {
        crate::thread::spawn(fn_ptr, stack, 0, tls, child_tid);
    }

    tid as i64
}

/// epoll_wait: yield once, then check for ready events.
/// Returns 0 (no events / timeout) so the caller runs its housekeeping loop.
/// struct epoll_event { u32 events; u64 data; } = 12 bytes
fn sys_epoll_wait(_epfd: u64, events_ptr: u64, maxevents: u64) -> i64 {
    crate::thread::yield_to_other();

    // Check for pipe data
    let mut count = 0i64;
    let max = maxevents as usize;
    const EPOLLIN: u32 = 0x001;

    if count < max as i64 && crate::pipe::any_has_data() {
        unsafe {
            let ev = events_ptr as *mut u8;
            *(ev as *mut u32) = EPOLLIN;
            *((ev as u64 + 4) as *mut u64) = 200;
        }
        count += 1;
    }

    count
}

/// ppoll: check pollfds for ready pipe fds.
/// struct pollfd { int fd; short events; short revents; } — 8 bytes each.
fn sys_ppoll(fds_ptr: u64, nfds: u64) -> i64 {
    crate::thread::yield_to_other();

    // Check pollfds and return -EINTR if nothing ready
    let mut ready = 0i64;
    const POLLIN: u16 = 0x0001;

    for i in 0..nfds as usize {
        // SAFETY: fds_ptr is identity-mapped user memory.
        unsafe {
            let pfd = (fds_ptr + (i as u64) * 8) as *mut u8;
            let fd = *(pfd as *const i32);
            let events = *((pfd as u64 + 4) as *const u16);
            // Clear revents
            *((pfd as u64 + 6) as *mut u16) = 0;
            if (events & POLLIN) != 0 {
                let has_data = if fd == 0 {
                    // stdin: check COM1 LSR bit 0
                    (x86_64::instructions::port::Port::<u8>::new(0x3FD).read() & 1) != 0
                } else if crate::pipe::is_pipe_fd(fd) {
                    crate::pipe::has_data(fd)
                } else {
                    false
                };
                if has_data {
                    *((pfd as u64 + 6) as *mut u16) = POLLIN;
                    ready += 1;
                }
            }
        }
    }
    ready // 0 = no ready fds
}

fn sys_fcntl(fd: i32, cmd: i32, arg: u64) -> i64 {
    const F_GETFL: i32 = 3;
    const F_SETFL: i32 = 4;
    const O_NONBLOCK: u64 = 0x800;

    match cmd {
        F_GETFL => 0, // report no flags
        F_SETFL => {
            // Track O_NONBLOCK for pipe fds
            if crate::pipe::is_pipe_fd(fd) {
                crate::pipe::set_nonblock(fd, (arg & O_NONBLOCK) != 0);
            }
            0
        }
        _ => 0, // other fcntl commands: no-op
    }
}

fn sys_futex(uaddr: u64, op: u64, val: u64) -> i64 {
    let cmd = (op & 0x7f) as u32; // mask FUTEX_PRIVATE_FLAG
    match cmd {
        0 | 9 => {
            // FUTEX_WAIT / FUTEX_WAIT_BITSET: block until value changes.
            // Sleep the thread and let the timer wake it periodically
            // to recheck. This prevents the thread from consuming CPU
            // while waiting for a lock holder to release.
            let current = unsafe { *(uaddr as *const u32) };
            if current != val as u32 {
                return -11; // -EAGAIN
            }
            crate::thread::futex_sleep(uaddr, val as u32);
            0
        }
        1 => {
            // FUTEX_WAKE: wake sleeping threads, then yield N times
            // to give EVERY thread a chance to see the value change
            // before any thread can re-lock the mutex.
            let woken = crate::thread::futex_wake(uaddr, val as u32);
            // Yield to all threads so the waiter sees the unlock
            for _ in 0..crate::thread::num_threads() {
                crate::thread::yield_to_other();
            }
            if woken > 0 { woken } else { 1 }
        }
        _ => 0,
    }
}

fn sys_readlink(path: *const u8, buf: *mut u8, bufsiz: usize) -> i64 {
    // Check if it's /proc/self/exe
    let exe = b"/proc/self/exe";
    let is_self_exe = unsafe {
        (0..exe.len()).all(|i| *path.add(i) == exe[i]) && *path.add(exe.len()) == 0
    };

    if is_self_exe {
        let target = b"/otp/erts-15.2.7/bin/beam.smp";
        let len = target.len().min(bufsiz);
        // SAFETY: buf is identity-mapped user memory.
        unsafe { core::ptr::copy_nonoverlapping(target.as_ptr(), buf, len); }
        len as i64
    } else {
        -22 // -EINVAL
    }
}

/// Last returned nanosecond value — ensures monotonicity.
static LAST_TIME_NS: AtomicU64 = AtomicU64::new(0);

/// Return a monotonically increasing nanosecond value. Used by both
/// clock_gettime and the RDTSC trap handler. Guaranteed to return a
/// value strictly greater than any previously returned value (across
/// all threads, even with preemptive scheduling).
pub fn monotonic_ns() -> u64 {
    // Read RDTSC with interrupts disabled to prevent preemption
    // between TSC read and CAS — this is the key to preventing
    // apparent time reversals.
    let were_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    let tsc = unsafe { core::arch::x86_64::_rdtsc() };
    let total_ns = tsc / 2;
    let mut last = LAST_TIME_NS.load(Ordering::SeqCst);
    let result = loop {
        let new = total_ns.max(last + 1);
        match LAST_TIME_NS.compare_exchange(last, new, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => break new,
            Err(actual) => last = actual,
        }
    };
    if were_enabled { x86_64::instructions::interrupts::enable(); }
    result
}

fn sys_clock_gettime(_clk_id: i32, tp: *mut u64) -> i64 {
    if tp.is_null() {
        return -22; // -EINVAL
    }
    let ns = monotonic_ns();
    unsafe {
        *tp = ns / 1_000_000_000;
        *(tp.add(1)) = ns % 1_000_000_000;
    }
    0
}

#[repr(C)]
struct IoVec {
    base: *const u8,
    len: usize,
}

fn sys_writev(fd: i32, iov: *const IoVec, iovcnt: usize) -> i64 {
    let mut total = 0i64;
    for i in 0..iovcnt {
        // SAFETY: iov array is in identity-mapped user memory.
        let v = unsafe { &*iov.add(i) };
        let written = sys_write(fd, v.base, v.len);
        if written < 0 {
            return written;
        }
        total += written;
    }
    total
}

fn sys_getcwd(buf: *mut u8, size: usize) -> i64 {
    if size >= 2 {
        // SAFETY: buf points to user memory.
        unsafe {
            *buf = b'/';
            *buf.add(1) = 0;
        }
        1
    } else {
        -34 // -ERANGE
    }
}

/// Jump to a userspace entry point with a given stack pointer.
///
/// The stack must already contain the argc/argv/envp/auxv layout expected by
/// musl _start. We jump directly — no fake return address is pushed, matching
/// how the Linux kernel transfers control to ELF entry points.
pub fn jump_to_user(entry: u64, user_stack_top: u64) -> ! {
    serial_println!("[user] jumping to {:#x} sp={:#x}", entry, user_stack_top);
    // SAFETY: entry is the ELF entry point, user_stack_top is a valid stack
    // with argc at [rsp].
    unsafe {
        core::arch::asm!(
            "mov rsp, {sp}",
            "xor rbp, rbp",
            "jmp {entry}",
            sp = in(reg) user_stack_top,
            entry = in(reg) entry,
            options(noreturn),
        );
    }
}
