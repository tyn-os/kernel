//! Linux syscall handler — traps `syscall` via LSTAR MSR.
//!
//! Assembly stub saves registers on a kernel stack, calls Rust dispatcher,
//! restores registers, returns via `sysretq`. Follows Kerla/rCore pattern.

use crate::serial_println;
use core::arch::global_asm;

/// Initialize the syscall entry point via MSRs.
/// Per-CPU data for syscall entry, accessed via GS segment.
/// Layout: [0]=kernel_stack, [8]=scratch, [16]=saved_r9, [24]=saved_clone_rip
#[repr(C, align(64))]
struct PerCpuSyscall {
    kernel_stack: u64,  // gs:[0]
    scratch: u64,       // gs:[8]
    saved_r9: u64,      // gs:[16] — R9 at syscall entry (fn ptr for clone)
    saved_clone_rip: u64, // gs:[24] — RCX at syscall entry (return addr for child)
}

const MAX_CPUS: usize = 16;

/// Per-CPU syscall data for up to 16 CPUs.
static mut PERCPU_SYSCALL: [PerCpuSyscall; MAX_CPUS] = {
    const EMPTY: PerCpuSyscall = PerCpuSyscall { kernel_stack: 0, scratch: 0, saved_r9: 0, saved_clone_rip: 0 };
    [EMPTY; MAX_CPUS]
};

/// Set up syscall MSRs on the current CPU. Must be called on every CPU.
/// Sets LSTAR, STAR, SFMASK, EFER.SCE, and GS_BASE for per-CPU data.
pub fn init_cpu_msrs(cpu_id: usize) {
    unsafe {
        use x86_64::registers::model_specific::Msr;

        // STAR: kernel CS/SS in bits 47:32. User segment not used (ring 0).
        Msr::new(0xC000_0081).write(0x10u64 << 32);

        // LSTAR: syscall entry point.
        Msr::new(0xC000_0082).write(syscall_entry as u64);

        // SFMASK: clear IF on syscall entry.
        Msr::new(0xC000_0084).write(0x200);

        // Enable SCE (System Call Enable) in EFER.
        let mut efer = Msr::new(0xC000_0080);
        let val = efer.read();
        efer.write(val | 1);

        // Set GS_BASE to this CPU's per-CPU syscall data.
        // The syscall entry stub uses gs:0 for kernel stack and gs:8 for scratch.
        let gs_base = &raw const PERCPU_SYSCALL[cpu_id] as u64;
        Msr::new(0xC000_0101).write(gs_base); // IA32_GS_BASE
    }
}

pub fn init() {
    init_cpu_msrs(0);
    // Initialize kernel stack pointer for CPU 0 (thread 0's stack).
    unsafe {
        extern "C" { static syscall_stack_0_top: u8; }
        let kstack = &syscall_stack_0_top as *const u8 as u64;
        PERCPU_SYSCALL[0].kernel_stack = kstack;
    }
    serial_println!("[syscall] MSRs configured");
}

/// Update the per-CPU kernel stack pointer (called from scheduler on context switch).
pub fn set_current_kernel_stack(kstack: u64) {
    let cpu = unsafe {
        let apic_id = *((0xFEE0_0020u64) as *const u32) >> 24;
        apic_id as usize
    };
    unsafe {
        PERCPU_SYSCALL[cpu].kernel_stack = kstack;
    }
}

/// Read saved R9 and clone RIP from this CPU's per-CPU data.
/// These are saved at syscall entry and must be per-CPU to avoid SMP races.
pub fn get_clone_regs() -> (u64, u64) {
    let cpu = unsafe {
        let apic_id = *((0xFEE0_0020u64) as *const u32) >> 24;
        apic_id as usize
    };
    unsafe {
        (PERCPU_SYSCALL[cpu].saved_r9, PERCPU_SYSCALL[cpu].saved_clone_rip)
    }
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
    ".global last_syscall_ret",
    "last_syscall_ret: .quad 0",
    ".global timer_active",
    "timer_active: .byte 0",
    ".section .text",

    ".global syscall_entry",
    "syscall_entry:",
    // rcx = user return RIP (clobbered by syscall instruction)
    // r11 = user RFLAGS (clobbered by syscall instruction)
    // GS_BASE points to per-CPU data: [0]=kernel_stack, [8]=scratch
    "mov gs:[8], rsp",           // save user RSP to per-CPU scratch
    "mov rsp, gs:[0]",           // load per-CPU kernel stack
    "push qword ptr gs:[8]",     // push saved user RSP onto kernel stack
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
    // Save R9 for clone (musl passes fn in R9) and RCX (return RIP for child)
    // Per-CPU via GS to avoid SMP race when both CPUs are in syscall handlers
    "mov gs:[16], r9",
    "mov gs:[24], rcx",
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
    // Save kernel stack top to per-CPU data for next syscall
    "lea r11, [rsp + 8]",
    "mov gs:[0], r11",           // per-CPU kernel stack update
    "pop rsp",
    "sti",  // Re-enable interrupts before returning to user
    "jmp rcx",  // Return to user code
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
    let idx = crate::sched::current_idx();
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
    let idx = crate::sched::current_idx();
    {
        static SC: AtomicU64 = AtomicU64::new(0);
        let c = SC.fetch_add(1, Ordering::Relaxed);
        let _ = c; // logging disabled for clean ERTS output
    }
    if idx < 24 { IN_SYSCALL[idx].store(true, Ordering::Relaxed); }
    let result = syscall_dispatch_inner(nr, a0, a1, a2, a3, _a4);
    if idx < 24 { IN_SYSCALL[idx].store(false, Ordering::Relaxed); }
    crate::sched::check_resched();
    {
    }
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
            crate::net::socket::close(a0 as i32);
            crate::net::poll(); // flush FIN
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
        19 => { // readv — scatter read
            let iov = a1 as *const IoVec;
            let iovcnt = a2 as usize;
            let mut total = 0i64;
            for i in 0..iovcnt {
                let v = unsafe { &*iov.add(i) };
                let n = sys_read(a0 as i32, v.base as *mut u8, v.len);
                if n < 0 { if total == 0 { return n; } else { break; } }
                total += n;
                if (n as usize) < v.len { break; }
            }
            total
        }
        SYS_GETRANDOM => sys_getrandom(a0 as *mut u8, a1 as usize),
        SYS_GETCWD => sys_getcwd(a0 as *mut u8, a1 as usize),
        SYS_TKILL | SYS_TGKILL => 0,
        SYS_SCHED_SETAFFINITY => 0,
        SYS_SOCKETPAIR => sys_pipe(a3 as *mut i32), // fake as pipe pair
        SYS_PRCTL => 0, // no-op
        SYS_FUTEX => sys_futex(a0, a1, a2),
        SYS_PPOLL => sys_ppoll(a0, a1),
        SYS_SELECT => {
            // select(0, NULL, NULL, NULL, ...) is ERTS's idle poll.
            // Don't yield — just return immediately so the scheduler loop runs fast.
            if a0 == 0 {
                0
            } else {
                crate::net::poll();
                crate::sched::yield_current();
                0
            }
        }
        SYS_TIMERFD_SETTIME => 0,
        SYS_SCHED_YIELD => {
            crate::sched::yield_current();
            0
        }
        SYS_NANOSLEEP => { crate::sched::yield_current(); 0 }
        SYS_FORK => -38, // -ENOSYS
        SYS_CLONE => {
            // Allow clone but log. With our single-scheduler ERTS patch,
            // only 2 auxiliary threads are created (signal handler + poll).
            sys_clone(a0, a1, a2, a3, _a4)
        }
        SYS_EXIT => sys_exit_group(a0 as i32),
        // Socket syscalls
        41 => crate::net::socket::sys_socket(a0 as i32, a1 as i32, a2 as i32),
        42 => -115, // connect → -EINPROGRESS (TODO)
        43 | 288 => crate::net::socket::sys_accept(a0 as i32, a1 as *mut u8, a2 as *mut u32, a3 as i32),
        44 => crate::net::socket::sys_sendto(a0 as i32, a1 as *const u8, a2 as usize, a3 as i32, 0 as *const u8, 0),
        45 => crate::net::socket::sys_recvfrom(a0 as i32, a1 as *mut u8, a2 as usize, a3 as i32, 0 as *mut u8, 0 as *mut u32),
        46 => { // sendmsg — used by erl_child_setup protocol
            // Parse msghdr to get total iov length for return value
            // struct msghdr { void *name; socklen_t namelen; struct iovec *iov;
            //                 size_t iovlen; void *control; size_t controllen; int flags; }
            let iov_ptr = unsafe { *((a1 + 16) as *const u64) };
            let iov_len = unsafe { *((a1 + 24) as *const u64) };
            let mut total = 0i64;
            for i in 0..iov_len {
                let len = unsafe { *((iov_ptr + i * 16 + 8) as *const u64) };
                total += len as i64;
            }
            // Write the data to the pipe if it's a pipe fd
            if crate::pipe::is_pipe_fd(a0 as i32) {
                let base = unsafe { *((iov_ptr) as *const u64) };
                crate::pipe::write(a0 as i32, base as *const u8, total as usize);
            }
            total
        }
        47 => -11, // recvmsg → -EAGAIN
        48 => 0,   // shutdown → success
        49 => crate::net::socket::sys_bind(a0 as i32, a1 as *const u8, a2 as u32),
        50 => crate::net::socket::sys_listen(a0 as i32, a1 as i32),
        51 => crate::net::socket::sys_getsockname(a0 as i32, a1 as *mut u8, a2 as *mut u32),
        52 => crate::net::socket::sys_getpeername(a0 as i32, a1 as *mut u8, a2 as *mut u32),
        54 => crate::net::socket::sys_setsockopt(a0 as i32, a1 as i32, a2 as i32, a3 as *const u8, _a4 as u32),
        55 => crate::net::socket::sys_getsockopt(a0 as i32, a1 as i32, a2 as i32, a3 as *mut u8, _a4 as *mut u32),
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
        // Auto-respond on erl_child_setup response pipe.
        // When writing to the command pipe (odd fd like 205, 207, ...),
        // simulate erl_child_setup by writing a 0 byte to the response
        // pipe (fd - 2, which is the write end of the response pipe).
        if fd >= 205 && fd % 2 == 1 && crate::pipe::is_pipe_fd(fd - 2) {
            let r = crate::pipe::write(fd - 2, [0u8].as_ptr(), 1);
            serial_println!("[auto-resp] wrote {} byte(s) to fd {}", r, fd - 2);
        }
        result
    } else if crate::net::socket::is_socket_fd(fd) {
        let r = crate::net::socket::sys_sendto(fd, buf, count, 0, core::ptr::null(), 0);
        crate::net::poll(); // flush smoltcp's tx buffer
        r
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
        crate::sched::yield_current();
        if count >= 8 {
            unsafe { *(buf as *mut u64) = 1; }
        }
        return 8;
    }
    // Socket read
    if crate::net::socket::is_socket_fd(fd) {
        return crate::net::socket::sys_recvfrom(fd, buf, count, 0, core::ptr::null_mut(), core::ptr::null_mut());
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
        crate::sched::yield_current();
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

    // Log failed opens for .beam files (debugging standard_error loading)
    if path.len() > 5 && &path[path.len()-5..] == b".beam" {
        if let Ok(s) = core::str::from_utf8(path) {
            serial_println!("[vfs] ENOENT {}", s);
        }
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

    // musl's __clone puts arg at [stack] and passes fn through R9.
    // Read from per-CPU GS data (set at syscall entry).
    let (fn_ptr, _) = get_clone_regs();

    // saved_clone_rip is set in the assembly stub (RCX = return RIP from syscall)
    // The child reads it in clone_child_return to return to musl's __clone

    // Use the SMP scheduler to create the thread
    let tid = crate::sched::spawn(fn_ptr, stack, tls, child_tid);

    // CLONE_PARENT_SETTID (0x00100000): write TID to parent_tid.
    if (flags & 0x00100000) != 0 && parent_tid != 0 {
        unsafe { *(parent_tid as *mut u32) = tid; }
    }
    // CLONE_CHILD_SETTID (0x01000000): write TID to child_tid.
    // Must happen before clone returns so the parent sees it immediately.
    if (flags & 0x01000000) != 0 && child_tid != 0 {
        unsafe { *(child_tid as *mut u32) = tid; }
    }

    serial_println!("[clone] tid={} fn={:#x}", tid, fn_ptr);
    tid as i64
}

/// epoll_wait: yield once, then check for ready events.
/// Returns 0 (no events / timeout) so the caller runs its housekeeping loop.
/// struct epoll_event { u32 events; u64 data; } = 12 bytes
fn sys_epoll_wait(_epfd: u64, events_ptr: u64, maxevents: u64) -> i64 {
    crate::net::poll();
    crate::sched::yield_current();

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

    // Check socket readiness (for gen_tcp accept/recv)
    if count < max as i64 && crate::net::socket::any_socket_ready() {
        unsafe {
            let ev = (events_ptr + (count as u64) * 12) as *mut u8;
            *(ev as *mut u32) = EPOLLIN;
            *((ev as u64 + 4) as *mut u64) = 500; // socket fd base
        }
        count += 1;
    }

    count
}

/// ppoll: check pollfds for ready pipe fds.
/// struct pollfd { int fd; short events; short revents; } — 8 bytes each.
fn sys_ppoll(fds_ptr: u64, nfds: u64) -> i64 {
    // Poll the network stack so smoltcp processes packets and updates
    // socket readiness before we check pollfds.
    crate::net::poll();

    crate::sched::yield_current();

    // Debug: log first ppoll that includes socket fds
    {
        static LOGGED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
        if !LOGGED.load(Ordering::Relaxed) {
            for i in 0..nfds as usize {
                let fd = unsafe { *((fds_ptr + (i as u64) * 8) as *const i32) };
                if fd >= 500 {
                    LOGGED.store(true, Ordering::Relaxed);
                    serial_println!("[ppoll] nfds={} socket_fd={} at idx={}", nfds, fd, i);
                    break;
                }
            }
        }
    }

    // Check pollfds
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
            if events != 0 {
                let revents = if fd == 0 {
                    // stdin: check COM1 LSR bit 0
                    if (events & POLLIN) != 0 && (x86_64::instructions::port::Port::<u8>::new(0x3FD).read() & 1) != 0 {
                        POLLIN
                    } else { 0 }
                } else if crate::pipe::is_pipe_fd(fd) {
                    if (events & POLLIN) != 0 && crate::pipe::has_data(fd) { POLLIN } else { 0 }
                } else if crate::net::socket::is_socket_fd(fd) {
                    crate::net::socket::poll_socket(fd) & events
                } else {
                    0
                };
                if revents != 0 {
                    *((pfd as u64 + 6) as *mut u16) = revents;
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
            // FUTEX_WAIT / FUTEX_WAIT_BITSET: atomically check-and-sleep
            // under a per-address spinlock (SMP-safe).
            crate::sched::futex_wait(uaddr, val as u32)
        }
        1 => {
            // FUTEX_WAKE: wake up to `val` threads, send IPI if target idle.
            crate::sched::futex_wake(uaddr, val as u32)
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

/// TSC frequency in Hz, calibrated against PIT at boot.
static TSC_FREQ_HZ: AtomicU64 = AtomicU64::new(2_000_000_000); // default 2 GHz

/// Per-CPU TSC offset (signed). TSC_offset[cpu] = BSP_TSC - AP_TSC at sync point.
/// Adding this to an AP's raw TSC normalizes it to the BSP's epoch.
static TSC_OFFSETS: [core::sync::atomic::AtomicI64; 16] = {
    const ZERO: core::sync::atomic::AtomicI64 = core::sync::atomic::AtomicI64::new(0);
    [ZERO; 16]
};

/// Calibrate TSC frequency against PIT channel 2. Call once at boot (BSP only).
pub fn calibrate_tsc() {
    unsafe {
        // PIT frequency = 1,193,182 Hz. 10ms = 11932 PIT ticks.
        let pit_count: u16 = 11932;

        // Program PIT channel 2 in one-shot mode
        let val = x86_64::instructions::port::Port::<u8>::new(0x61).read();
        x86_64::instructions::port::Port::<u8>::new(0x61).write(val & 0xFC);
        x86_64::instructions::port::Port::<u8>::new(0x43).write(0xB0);
        x86_64::instructions::port::Port::<u8>::new(0x42).write((pit_count & 0xFF) as u8);
        x86_64::instructions::port::Port::<u8>::new(0x42).write((pit_count >> 8) as u8);
        x86_64::instructions::port::Port::<u8>::new(0x61).write((val & 0xFC) | 0x01);

        let tsc_start = core::arch::x86_64::_rdtsc();

        while (x86_64::instructions::port::Port::<u8>::new(0x61).read() & 0x20) == 0 {
            core::hint::spin_loop();
        }

        let tsc_end = core::arch::x86_64::_rdtsc();
        let tsc_ticks = tsc_end - tsc_start;

        // tsc_freq = tsc_ticks * pit_hz / pit_count
        let freq = tsc_ticks * 1_193_182 / pit_count as u64;
        TSC_FREQ_HZ.store(freq, Ordering::Release);

        crate::serial_println!("[time] TSC frequency: {} MHz ({} ticks/10ms)",
            freq / 1_000_000, tsc_ticks);
    }
}

/// Measure and store the TSC offset for an AP relative to BSP.
/// Uses trampoline memory at 0x8030/0x8038 as shared sync vars.
/// Call from BSP after AP has called ap_tsc_sync().
pub fn measure_tsc_offset(cpu_id: usize) {
    let sync_state = 0x8030u64 as *mut u64;
    let ap_tsc_ptr = 0x8038u64 as *mut u64;

    let mut offsets = [0i64; 3];
    for round in 0..3 {
        unsafe { core::ptr::write_volatile(sync_state, 0); }
        core::sync::atomic::fence(Ordering::SeqCst);

        // Signal AP to read its TSC
        unsafe { core::ptr::write_volatile(sync_state, 1); }
        let bsp_tsc = unsafe { core::arch::x86_64::_rdtsc() };

        // Wait for AP
        while unsafe { core::ptr::read_volatile(sync_state) } != 2 {
            core::hint::spin_loop();
        }

        let ap_tsc = unsafe { core::ptr::read_volatile(ap_tsc_ptr) };
        offsets[round] = bsp_tsc as i64 - ap_tsc as i64;
    }

    offsets.sort();
    let offset = offsets[1]; // median

    TSC_OFFSETS[cpu_id].store(offset, Ordering::Release);
    crate::serial_println!("[time] CPU {} TSC offset: {} ticks", cpu_id, offset);

    // Signal done
    unsafe { core::ptr::write_volatile(sync_state, 3); }
}

/// AP side of TSC offset measurement. Call from AP after GDT/IDT setup.
pub fn ap_tsc_sync() {
    let sync_state = 0x8030u64 as *mut u64;
    let ap_tsc_ptr = 0x8038u64 as *mut u64;

    for _round in 0..3 {
        while unsafe { core::ptr::read_volatile(sync_state) } != 1 {
            core::hint::spin_loop();
        }

        let my_tsc = unsafe { core::arch::x86_64::_rdtsc() };
        unsafe { core::ptr::write_volatile(ap_tsc_ptr, my_tsc); }
        unsafe { core::ptr::write_volatile(sync_state, 2); }

        // Wait for BSP to reset for next round
        while unsafe { core::ptr::read_volatile(sync_state) } == 2 {
            core::hint::spin_loop();
        }
    }
}

/// Return a monotonically increasing nanosecond value.
/// Uses per-CPU TSC offset correction and a global ratchet.
pub fn monotonic_ns() -> u64 {
    let were_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();

    let raw_tsc = unsafe { core::arch::x86_64::_rdtsc() };

    // Apply per-CPU TSC offset to normalize to BSP epoch
    let cpu = unsafe { *((0xFEE0_0020u64) as *const u32) >> 24 } as usize;
    let offset = if cpu < 16 { TSC_OFFSETS[cpu].load(Ordering::Relaxed) } else { 0 };
    let corrected_tsc = (raw_tsc as i64 + offset) as u64;

    // Convert to nanoseconds: ns = tsc * 1_000_000_000 / freq
    // To avoid overflow, use: ns = tsc / (freq / 1_000_000_000)
    // But freq/1B might be < 1 for GHz clocks. Use: ns = tsc * 1000 / (freq / 1_000_000)
    let freq_mhz = TSC_FREQ_HZ.load(Ordering::Relaxed) / 1_000_000;
    let total_ns = if freq_mhz > 0 { corrected_tsc * 1000 / freq_mhz } else { corrected_tsc / 2 };

    // Ratchet: never go backwards
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
            "sti",  // Enable interrupts for ERTS execution
            "jmp {entry}",
            sp = in(reg) user_stack_top,
            entry = in(reg) entry,
            options(noreturn),
        );
    }
}
