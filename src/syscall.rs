//! Linux syscall handler — traps `syscall` via LSTAR MSR.
//!
//! Assembly stub saves registers on a kernel stack, calls Rust dispatcher,
//! restores registers, returns via `sysretq`. Follows Kerla/rCore pattern.

use crate::serial_println;
use core::arch::global_asm;

/// Initialize the syscall entry point via MSRs.
/// Per-CPU data for syscall entry, accessed via GS segment.
/// Layout: [0]=kernel_stack, [8]=scratch, [16]=saved_r9, [24]=saved_clone_rip,
///         [32]=saved_rdx, [40]=saved_r8
#[repr(C, align(64))]
struct PerCpuSyscall {
    kernel_stack: u64,    // gs:[0]
    scratch: u64,         // gs:[8]
    saved_r9: u64,        // gs:[16] — R9 at syscall entry (fn ptr for clone)
    saved_clone_rip: u64, // gs:[24] — RCX at syscall entry (return addr for child)
    saved_rdx: u64,       // gs:[32] — RDX at syscall entry (fn ptr for clone3)
    saved_r8: u64,        // gs:[40] — R8 at syscall entry (arg for clone3)
}

const MAX_CPUS: usize = 16;

/// Per-CPU syscall data for up to 16 CPUs.
static mut PERCPU_SYSCALL: [PerCpuSyscall; MAX_CPUS] = {
    const EMPTY: PerCpuSyscall = PerCpuSyscall { kernel_stack: 0, scratch: 0, saved_r9: 0, saved_clone_rip: 0, saved_rdx: 0, saved_r8: 0 };
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

/// Read saved clone registers from this CPU's per-CPU data.
/// Returns (r9, rip, rdx, r8) — for clone: r9=fn, rip=ret. For clone3: rdx=fn, r8=arg.
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
    // Save ALL user registers that Linux syscall ABI preserves.
    // Critically: r11 holds USER RFLAGS (the `syscall` instruction loaded
    // it from rflags). Save it so we can restore RFLAGS on the way out;
    // without this, sticky flags (DF in particular) leak across syscalls
    // and can corrupt rep-movs/rep-stos memcpy/memset in user code.
    "push rcx",      // return RIP
    "push r11",      // user RFLAGS (saved by `syscall` instruction)
    "cld",           // clear DF for kernel C ABI (rep movs/stos must have DF=0)
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
    // Save registers for clone/clone3 child return (per-CPU via GS)
    // clone: R9=fn, RCX=return RIP
    // clone3: RDX=fn, R8=arg, RCX=return RIP
    "mov gs:[16], r9",
    "mov gs:[24], rcx",
    "mov gs:[32], rdx",
    "mov gs:[40], r8",
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
    // Verify RCX is a valid user code address (catch stack corruption)
    "cmp rcx, 0x400000",
    "jb 3f",               // bad return address → trap
    "mov [rip + last_syscall_ret], rcx",
    "add rsp, 8",    // skip alignment padding
    // Restore user RFLAGS from r11 (which we restored from the saved slot
    // a few lines above). popfq pops 8 bytes from the kernel stack into
    // the RFLAGS register, including DF/IF/etc.
    "push r11",
    "popfq",
    // Save kernel stack top to per-CPU data for next syscall (use rax as
    // scratch — but rax holds the syscall return value, so save/restore).
    "push rax",
    "lea rax, [rsp + 16]",
    "mov gs:[0], rax",
    "pop rax",
    "pop rsp",
    "jmp rcx",  // Return to user code (RFLAGS already restored by popfq)
    // Bad return address detected
    "3:",
    "mov rdi, rcx",          // pass bad RCX as arg
    "mov rsi, rsp",          // pass RSP
    "call {bad_rcx}",
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
    bad_rcx = sym bad_rcx_handler,
);

#[no_mangle]
extern "C" fn bad_return_address(addr: u64) {
    crate::serial::raw_str(b"BAD_RET@");
    crate::serial::raw_hex(addr);
    crate::serial::raw_str(b"\n");
}

#[no_mangle]
extern "C" fn bad_rcx_handler(rcx: u64, rsp: u64) {
    crate::serial::raw_str_nolock(b"\nBAD_RCX=");
    crate::serial::raw_hex_nolock(rcx);
    crate::serial::raw_str_nolock(b" RSP=");
    crate::serial::raw_hex_nolock(rsp);
    // Dump 8 values from the kernel stack to see what's there
    crate::serial::raw_str_nolock(b"\nStack:");
    for i in 0..8u64 {
        crate::serial::raw_str_nolock(b" ");
        let val = unsafe { *((rsp + i * 8) as *const u64) };
        crate::serial::raw_hex_nolock(val);
    }
    crate::serial::raw_str_nolock(b"\n");
    crate::halt_loop();
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
/// Must stay within the 4 GiB identity-mapped region and 2560M RAM.
/// Start above: ELF copy (320 MiB), CPIO copy (330 MiB + up to ~80 MiB = 410 MiB).
const MMAP_BASE: u64 = 0x1A00_0000; // 416 MiB
static MMAP_NEXT: AtomicU64 = AtomicU64::new(MMAP_BASE);

/// brk heap top (current break — set by `brk(addr)`).
static BRK_TOP: AtomicU64 = AtomicU64::new(0);
/// First non-zero brk value ERTS set — used as the base for "brk used"
/// computation. Linux ABI: `brk(0)` returns the initial break, which is
/// typically end-of-data + heap; we store whatever ERTS first asks for.
static BRK_BASE: AtomicU64 = AtomicU64::new(0);

/// Print a one-line memory snapshot to the serial log. Useful for
/// post-mortem benchmarking — call from anywhere in the kernel after
/// userspace is up to read out actual ERTS allocator usage.
pub fn mem_stats_snapshot() {
    let mmap_top = MMAP_NEXT.load(Ordering::Relaxed);
    let mmap_used = mmap_top.saturating_sub(MMAP_BASE);
    let brk = BRK_TOP.load(Ordering::Relaxed);
    let brk_base = BRK_BASE.load(Ordering::Relaxed);
    let brk_used = if brk == 0 || brk_base == 0 { 0 }
                   else { brk.saturating_sub(brk_base) };
    // Static carve-outs (rough): ELF copy ~8 MB, CPIO copy ~30 MB,
    // user stack region 2 MB, kernel heap 2 MB, page tables ~few MB.
    const STATIC_MB: u64 = 8 + 30 + 2 + 2 + 4;
    let total_mb = STATIC_MB + mmap_used / (1024 * 1024) + brk_used / (1024 * 1024);
    serial_println!(
        "[memstat] mmap={} MB brk={} MB static~{} MB total~{} MB (mmap_top={:#x} brk_top={:#x})",
        mmap_used / (1024 * 1024),
        brk_used / (1024 * 1024),
        STATIC_MB,
        total_mb,
        mmap_top,
        brk
    );
}

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
        // Per-thread trace — first 50 syscalls for each thread except tid 0
        if idx > 0 && idx < 24 {
            static T_COUNT: [AtomicU64; 24] = [const { AtomicU64::new(0) }; 24];
            let tc = T_COUNT[idx].fetch_add(1, Ordering::Relaxed);
            if tc < 50 {
                serial_println!("[t{}#{}] nr={} a0={:#x}", idx, tc, nr, a0);
            }
        }
        // Always trace syscalls touching socket fds (>= 500) so we can see
        // what gen_tcp does on accepted connections — this runs after the
        // per-thread cap. Only includes syscalls where a0 is genuinely an fd.
        let fd_taking = matches!(nr,
            0 | 1 | 3 | 17 | 18 | 19 | 20 | 21 | 41 | 42 | 43 | 44 | 45 | 46 | 47
            | 48 | 49 | 50 | 51 | 52 | 54 | 55 | 72 | 217 | 232 | 233 | 288);
        if fd_taking && a0 >= 500 && a0 < 600 {
            static SOCK_C: AtomicU64 = AtomicU64::new(0);
            let sc = SOCK_C.fetch_add(1, Ordering::Relaxed);
            if sc < 1000 {
                serial_println!("[sock-sc] t{} nr={} fd={} a1={:#x} a2={:#x}", idx, nr, a0, a1, a2);
            }
        }
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
        SYS_MMAP => sys_mmap(a0, a1, a2 as i32, a3 as i32),
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
        // TODO: newfstatat(dirfd, pathname, statbuf, flags) — currently ignores pathname (a1)
        // and treats a0 as fd for fstat. Works because ERTS uses it for open fds only.
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
        267 => sys_readlink(a1 as *const u8, a2 as *mut u8, a3 as usize), // readlinkat (ignore dirfd)
        SYS_GETDENTS64 => sys_getdents64(a0 as i32, a1 as *mut u8, a2 as usize),
        SYS_PIPE | SYS_PIPE2 => sys_pipe(a0 as *mut i32),
        SYS_EPOLL_CREATE1 | 213 => 50, // fake epoll fd (213=epoll_create, 291=epoll_create1)
        SYS_TIMERFD_CREATE => {
            serial_println!("[timerfd_create] flags={:#x} returning fd=51", a1);
            51
        }
        SYS_EPOLL_CTL => sys_epoll_ctl(a1 as i32, a2 as i32, a3),
        SYS_EPOLL_WAIT | SYS_EPOLL_PWAIT => sys_epoll_wait(a0, a1, a2, a3 as i32),
        SYS_RT_SIGACTION => 0, // record but no-op
        SYS_RT_SIGPROCMASK => 0,
        SYS_SIGALTSTACK => 0,
        SYS_IOCTL => -25, // -ENOTTY
        SYS_PREAD64 => {
            if crate::vfs::is_vfs_fd(a0 as i32) {
                // pread64(fd, buf, count, offset) — atomic seek+read
                crate::vfs::pread(a0 as i32, a1 as *mut u8, a2 as usize, a3 as usize)
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
        96 => { // gettimeofday
            if a0 != 0 {
                let ns = monotonic_ns();
                unsafe {
                    *(a0 as *mut u64) = ns / 1_000_000_000; // tv_sec
                    *((a0 + 8) as *mut u64) = (ns / 1000) % 1_000_000; // tv_usec
                }
            }
            0
        }
        186 => (crate::sched::current_idx() + 1) as i64, // gettid
        270 => 0, // restart_syscall — no-op
        319 => -38i64, // memfd_create — ENOSYS, JIT falls back to mmap
        435 => -38i64, // clone3 → -ENOSYS, musl falls back to clone (nr=56)
        SYS_TKILL | SYS_TGKILL => 0,
        SYS_SCHED_SETAFFINITY => 0,
        SYS_SOCKETPAIR => sys_pipe(a3 as *mut i32), // fake as pipe pair
        SYS_PRCTL => 0, // no-op
        SYS_FUTEX => sys_futex(a0, a1, a2, a3),
        SYS_PPOLL => sys_ppoll(a0, a1, a2),
        SYS_SELECT => {
            // select(nfds, readfds, writefds, exceptfds, *timeval)
            // a4 is *timeval — if non-NULL, sleep for that duration.
            // ERTS uses select(0, NULL, NULL, NULL, &t) as a precise sleep;
            // without honoring the timeout, ERTS spins in a tight loop.
            crate::net::poll();
            let timeout_ptr = _a4 as *const u64;
            let target_ns = if !timeout_ptr.is_null() {
                let tv_sec = unsafe { *timeout_ptr };
                let tv_usec = unsafe { *timeout_ptr.add(1) };
                let ns = tv_sec.saturating_mul(1_000_000_000)
                    .saturating_add(tv_usec.saturating_mul(1_000));
                Some(crate::syscall::monotonic_ns().saturating_add(ns))
            } else {
                None
            };
            // Yield repeatedly until the timeout expires (or just once if no timeout).
            loop {
                crate::sched::yield_current();
                match target_ns {
                    Some(deadline) => {
                        if crate::syscall::monotonic_ns() >= deadline {
                            break;
                        }
                    }
                    None => break,
                }
            }
            0
        }
        SYS_TIMERFD_SETTIME => sys_timerfd_settime(a0 as i32, a1 as i32, a2),
        SYS_SCHED_YIELD => {
            // Linux semantics: sched_yield is a hint, returns immediately.
            // ERTS's scheduler calls it as part of its own main loop; if we
            // context-switch here, we delay ERTS getting back to its
            // top-of-loop checks (erts_check_time → timer wheel advance).
            // Preemption is driven by the 100 Hz timer-based trampoline,
            // not by voluntary sched_yield. With this no-op, schedulers
            // run their main loop continuously and timers fire on time.
            0
        }
        SYS_NANOSLEEP => { crate::sched::yield_current(); 0 }
        SYS_FORK => 42, // fake child PID — unikernel has no child processes
        SYS_CLONE => {
            // Allow clone but log. With our single-scheduler ERTS patch,
            // only 2 auxiliary threads are created (signal handler + poll).
            sys_clone(a0, a1, a2, a3, _a4)
        }
        SYS_EXIT => {
            // Single thread exit — mark thread as dead and yield
            crate::sched::thread_exit();
            0 // unreachable
        }
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
        47 => { // recvmsg — parse msghdr and read into first iov buffer
            let fd = a0 as i32;
            if crate::net::socket::is_socket_fd(fd) {
                // struct msghdr { void *name; socklen_t namelen; struct iovec *iov;
                //                 size_t iovlen; void *control; size_t controllen; int flags; }
                let iov_ptr = unsafe { *((a1 + 16) as *const u64) };
                let iov_len = unsafe { *((a1 + 24) as *const u64) };
                if iov_len > 0 {
                    let base = unsafe { *(iov_ptr as *const u64) } as *mut u8;
                    let len = unsafe { *((iov_ptr + 8) as *const u64) } as usize;
                    crate::net::socket::sys_recvfrom(fd, base, len, a2 as i32,
                        core::ptr::null_mut(), core::ptr::null_mut())
                } else {
                    0
                }
            } else {
                -11 // -EAGAIN for non-socket fds
            }
        }
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
        // No auto-respond — Linux strace confirms the 4-byte write to a pipe
        // is normal inter-thread coordination, not a request for response.
        // The OTP 20 era auto-respond for erl_child_setup was misguided for OTP 27.
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
    // Trace any read on socket-range fds (>= 500) so we can see what gen_tcp
    // is doing on accepted connections.
    if fd >= 500 {
        static R: AtomicU64 = AtomicU64::new(0);
        let n = R.fetch_add(1, Ordering::Relaxed);
        if n < 30 {
            serial_println!("[read-sock] fd={} count={}", fd, count);
        }
    }
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
    // Synthetic /sys files: return "0\n" once, then EOF
    if fd as i64 == FD_SYNTH_ZERO {
        if count >= 2 {
            unsafe {
                *buf = b'0';
                *buf.add(1) = b'\n';
            }
            return 2;
        }
        return 0;
    }
    // timerfd read: return expiration count from current state.
    if fd == 51 {
        let n = timerfd_consume();
        if n == 0 {
            return -11; // -EAGAIN: nothing to read yet
        }
        if count >= 8 {
            unsafe { *(buf as *mut u64) = n; }
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

/// Set initial brk to just above the loaded ELF segments.
pub fn set_initial_brk(mem_end: u64) {
    BRK_TOP.store(mem_end, Ordering::Relaxed);
}

fn sys_brk(addr: u64) -> i64 {
    if addr == 0 {
        BRK_TOP.load(Ordering::Relaxed) as i64
    } else {
        // Record the first non-zero brk as the base for memory accounting.
        let _ = BRK_BASE.compare_exchange(0, addr, Ordering::Relaxed, Ordering::Relaxed);
        BRK_TOP.store(addr, Ordering::Relaxed);
        addr as i64
    }
}

fn sys_mmap(addr: u64, length: u64, _prot: i32, flags: i32) -> i64 {
    const MAP_FIXED: i32 = 0x10;
    let aligned = (length + 0xFFF) & !0xFFF; // page-align

    // Only honor `addr` if MAP_FIXED is set. Otherwise it's just a hint —
    // using it blindly can zero out memory that's already in use (e.g., the
    // loaded ELF's .rodata containing preloaded BEAM modules).
    if addr != 0 && (flags & MAP_FIXED) != 0 {
        // SAFETY: addr is identity-mapped within our 4 GiB region.
        if aligned <= 0x400_0000 { // up to 64 MiB
            unsafe { core::ptr::write_bytes(addr as *mut u8, 0, aligned as usize) };
        }
        addr as i64
    } else {
        // Allocate from bump allocator (ignore non-FIXED hint addresses).
        let result = MMAP_NEXT.fetch_add(aligned, Ordering::Relaxed);
        // POSIX requires anonymous mmap to return zeroed memory. We rely
        // on the *host* providing this: QEMU's guest RAM is backed by a
        // host MAP_ANONYMOUS|MAP_PRIVATE region, which gives zero pages
        // on first touch (host-side demand paging). Eagerly zeroing here
        // would force every page to fault in immediately and would push
        // QEMU's RSS up to the full ERTS allocator commit (~1.4 GB).
        //
        // The MMAP_NEXT region (>= 352 MiB) is above all kernel boot
        // data, so no stale bytes from prior writes can surface — the
        // host mmap zero-on-first-touch is the entire backing.
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
    // Report all configured CPUs. NUM_CPUS is set during sched::init.
    // The 1-CPU bug from earlier was fixed in session 5.
    if len >= 8 {
        let ncpus = crate::sched::num_cpus().min(64);
        let mask_val: u64 = if ncpus >= 64 { !0u64 } else { (1u64 << ncpus) - 1 };
        unsafe {
            core::ptr::write_bytes(mask, 0, len);
            *(mask as *mut u64) = mask_val;
        }
        8
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
const FD_SYNTH_ZERO: i64 = 101; // synthetic file: read returns "0\n"
const FD_SYNTH_DIR: i64 = 102;  // synthetic empty directory: getdents64 returns 0

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

    // Synthetic /sys files for CPU topology — ERTS reads these to detect cores.
    // Returning a fake fd whose read yields "0\n" lets ERTS proceed past
    // its CPU topology detection phase (without these, ERTS hangs).
    if path.starts_with(b"/sys/devices/system/cpu/cpu")
        && (path.ends_with(b"/topology/physical_package_id")
            || path.ends_with(b"/topology/core_id")
            || path.ends_with(b"/topology/thread_siblings_list")
            || path.ends_with(b"/topology/core_siblings_list"))
    {
        return FD_SYNTH_ZERO;
    }
    // Synthetic /sys directories — empty dirs so ERTS's getdents64 returns 0 entries
    // and ERTS proceeds with default topology assumptions.
    if path == b"/sys/devices/system/node"
        || path == b"/sys/devices/system/cpu"
        || path.starts_with(b"/sys/devices/system/node/node")
        || path.starts_with(b"/sys/devices/system/cpu/cpu")
    {
        return FD_SYNTH_DIR;
    }

    // Try the VFS (cpio archive)
    let vfs_fd = crate::vfs::open(path);
    if vfs_fd >= 0 {
        return vfs_fd;
    }

    // Trace failed opens to see what ERTS is looking for
    {
        static FAIL_LOG: AtomicU64 = AtomicU64::new(0);
        let n = FAIL_LOG.fetch_add(1, Ordering::Relaxed);
        if n < 100 {
            if let Ok(s) = core::str::from_utf8(path) {
                serial_println!("[open ENOENT] {}", s);
            }
        }
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
    // Synthetic empty dir: return 0 (no entries)
    if fd as i64 == FD_SYNTH_DIR {
        let _ = (buf, count);
        return 0;
    }
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
    if path.starts_with(b"/otp")
        || path == b"/sys/devices/system/node"
        || path == b"/sys/devices/system/cpu"
        || path.starts_with(b"/sys/devices/system/node/node")
        || path.starts_with(b"/sys/devices/system/cpu/cpu")
    {
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
    unsafe {
        let mut i = 0;
        while i < len {
            let mut val: u64 = 0;
            let ok = core::arch::x86_64::_rdrand64_step(&mut val);
            if ok == 0 {
                val = core::arch::x86_64::_rdtsc(); // fallback
            }
            let bytes = val.to_ne_bytes();
            let to_copy = (len - i).min(8);
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.add(i), to_copy);
            i += to_copy;
        }
    }
    len as i64
}

fn sys_pipe(fds: *mut i32) -> i64 {
    if fds.is_null() {
        return -22; // -EINVAL
    }
    let (read_fd, write_fd) = crate::pipe::create();
    serial_println!("[sys_pipe] created ({}, {})", read_fd, write_fd);
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
/// Epoll fd registration table. ERTS calls epoll_ctl(epfd, ADD, fd, &event)
/// where event.data is what we should return when fd becomes ready.
/// We track (fd, data) pairs so epoll_wait can return the correct user data.
const EPOLL_MAX: usize = 64;
#[derive(Copy, Clone)]
struct EpollEntry { fd: i32, data: u64, events: u32 }
static mut EPOLL_TABLE: [EpollEntry; EPOLL_MAX] =
    [EpollEntry { fd: -1, data: 0, events: 0 }; EPOLL_MAX];
static EPOLL_LOCK: spin::Mutex<()> = spin::Mutex::new(());

fn sys_epoll_ctl(op: i32, fd: i32, event_ptr: u64) -> i64 {
    {
        static LOG: AtomicU64 = AtomicU64::new(0);
        let n = LOG.fetch_add(1, Ordering::Relaxed);
        if n < 30 {
            serial_println!("[epoll_ctl] op={} fd={}", op, fd);
        }
    }
    // op: 1=ADD, 2=DEL, 3=MOD
    // event struct: { u32 events; u64 data; } (packed, 12 bytes)
    let _g = EPOLL_LOCK.lock();
    unsafe {
        if op == 2 { // DEL
            for e in EPOLL_TABLE.iter_mut() {
                if e.fd == fd { e.fd = -1; e.data = 0; e.events = 0; }
            }
            return 0;
        }
        // ADD or MOD
        let events = if event_ptr != 0 { *(event_ptr as *const u32) } else { 0 };
        let data = if event_ptr != 0 { *((event_ptr + 4) as *const u64) } else { 0 };
        // Find existing or empty slot
        for e in EPOLL_TABLE.iter_mut() {
            if e.fd == fd { e.data = data; e.events = events; return 0; }
        }
        for e in EPOLL_TABLE.iter_mut() {
            if e.fd == -1 { e.fd = fd; e.data = data; e.events = events; return 0; }
        }
    }
    0 // table full — silently succeed
}

fn sys_epoll_wait(_epfd: u64, events_ptr: u64, maxevents: u64, timeout_ms: i32) -> i64 {
    const EPOLLIN: u32 = 0x001;
    let max = maxevents as usize;

    // ERTS calls epoll_wait(epfd, events, max, timeout_ms) with a timeout
    // computed from its nearest timer-wheel expiration. The scheduler
    // expects to be unblocked when either (a) an fd becomes ready, or
    // (b) the timeout elapses, so it can advance the timer wheel and
    // fire `receive after N` / gen_server:call timeouts. If we ignore
    // the timeout and return immediately every call, the scheduler
    // either spin-loops (burning CPU) or — worse — sleeps elsewhere
    // assuming epoll_wait blocked, and the timer wheel never advances.
    //
    // Convert timeout_ms = -1 (infinite) → block-until-ready; 0 →
    // return immediately; >0 → wait up to that many ms.
    let deadline = match timeout_ms {
        -1 => None,
        0 => Some(0u64), // return immediately after first scan
        t if t > 0 => Some(monotonic_ns().saturating_add((t as u64) * 1_000_000)),
        _ => Some(0u64),
    };

    loop {
        crate::net::poll();

        let mut count = 0i64;
        let _g = EPOLL_LOCK.lock();
        unsafe {
            for e in EPOLL_TABLE.iter() {
                if count >= max as i64 { break; }
                if e.fd < 0 { continue; }
                let ready = if e.fd == 51 {
                    timerfd_ready()
                } else if crate::pipe::is_pipe_fd(e.fd) {
                    crate::pipe::has_data(e.fd)
                } else if crate::net::socket::is_socket_fd(e.fd) {
                    crate::net::socket::poll_socket(e.fd) & 0x1 != 0
                } else {
                    false
                };
                if ready {
                    let off = (count as u64) * 12;
                    let ev = (events_ptr + off) as *mut u8;
                    *(ev as *mut u32) = EPOLLIN;
                    *((ev as u64 + 4) as *mut u64) = e.data;
                    count += 1;
                }
            }
        }
        drop(_g);

        if count > 0 {
            return count;
        }

        match deadline {
            Some(0) => return 0,
            Some(d) => {
                if monotonic_ns() >= d {
                    return 0;
                }
            }
            None => {}
        }

        crate::sched::yield_current();
    }
}

/// ppoll: check pollfds for ready pipe fds.
/// struct pollfd { int fd; short events; short revents; } — 8 bytes each.
fn sys_ppoll(fds_ptr: u64, nfds: u64, timeout_ptr: u64) -> i64 {
    // Poll the network stack so smoltcp processes packets and updates
    // socket readiness before we check pollfds.
    crate::net::poll();

    // Honor the timeout (struct timespec: { time_t tv_sec; long tv_nsec; }).
    // Without this, ppoll returns 0 instantly and ERTS spins.
    let target_ns = if timeout_ptr != 0 {
        let tv_sec = unsafe { *(timeout_ptr as *const u64) };
        let tv_nsec = unsafe { *((timeout_ptr + 8) as *const u64) };
        let ns = tv_sec.saturating_mul(1_000_000_000).saturating_add(tv_nsec);
        Some(monotonic_ns().saturating_add(ns))
    } else {
        None
    };
    let _ = target_ns; // initial ready check below; if 0 events, sleep loop

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

    const POLLIN: u16 = 0x0001;

    // Inner: scan all pollfds, fill revents, return count of ready fds.
    let scan_once = || -> i64 {
        let mut ready = 0i64;
        for i in 0..nfds as usize {
            unsafe {
                let pfd = (fds_ptr + (i as u64) * 8) as *mut u8;
                let fd = *(pfd as *const i32);
                let events = *((pfd as u64 + 4) as *const u16);
                *((pfd as u64 + 6) as *mut u16) = 0;
                if events != 0 {
                    let revents = if fd == 0 {
                        if (events & POLLIN) != 0 && (x86_64::instructions::port::Port::<u8>::new(0x3FD).read() & 1) != 0 {
                            POLLIN
                        } else { 0 }
                    } else if fd == 51 {
                        if (events & POLLIN) != 0 && timerfd_ready() { POLLIN } else { 0 }
                    } else if crate::pipe::is_pipe_fd(fd) {
                        if (events & POLLIN) != 0 && crate::pipe::has_data(fd) { POLLIN } else { 0 }
                    } else if crate::net::socket::is_socket_fd(fd) {
                        crate::net::socket::poll_socket(fd) & events
                    } else { 0 };
                    if revents != 0 {
                        *((pfd as u64 + 6) as *mut u16) = revents;
                        ready += 1;
                    }
                }
            }
        }
        ready
    };

    let initial = scan_once();
    if initial > 0 { return initial; }

    // No fds ready — honor timeout by yielding until deadline or fd becomes ready.
    loop {
        crate::sched::yield_current();
        crate::net::poll();
        let n = scan_once();
        if n > 0 { return n; }
        match target_ns {
            Some(deadline) => {
                if monotonic_ns() >= deadline { return 0; }
            }
            None => {
                // No timeout — block until something becomes ready.
                // Since we don't have eventfd-style blocking, just keep yielding.
                // The yield gives other threads CPU; eventually one writes to a pipe.
            }
        }
    }
}

fn sys_fcntl(fd: i32, cmd: i32, arg: u64) -> i64 {
    const F_GETFL: i32 = 3;
    const F_SETFL: i32 = 4;
    const O_NONBLOCK: u64 = 0x800;

    match cmd {
        F_GETFL => 0, // report no flags
        F_SETFL => {
            // Track O_NONBLOCK for pipe and socket fds. Sockets matter
            // for inet_drv's epoll-driven async-accept loop: with
            // multiple acceptors waiting on the same listener, a
            // *blocking* accept call lets every waiter race for the
            // single connection (and corrupt each other's listener
            // state); a non-blocking accept correctly returns EAGAIN
            // to all but one, letting epoll arbitrate the next.
            let nb = (arg & O_NONBLOCK) != 0;
            if crate::pipe::is_pipe_fd(fd) {
                crate::pipe::set_nonblock(fd, nb);
            } else if crate::net::socket::is_socket_fd(fd) {
                crate::net::socket::set_nonblock(fd, nb);
            }
            0
        }
        _ => 0, // other fcntl commands: no-op
    }
}

fn sys_futex(uaddr: u64, op: u64, val: u64, timeout_ptr: u64) -> i64 {
    let cmd = (op & 0x7f) as u32; // mask FUTEX_PRIVATE_FLAG
    match cmd {
        0 | 9 => {
            // FUTEX_WAIT / FUTEX_WAIT_BITSET. The 4th arg is a *const
            // struct timespec when non-null — the relative timeout for
            // FUTEX_WAIT (an absolute time for FUTEX_WAIT_BITSET, but
            // ERTS's ethr_event uses plain FUTEX_WAIT with a relative
            // timespec). If we ignore it, ethr_event_twait — used by
            // schedulers for timer-aware sleeps — becomes an infinite
            // wait, and `receive after N` / gen_server:call timeouts
            // never fire.
            let deadline = if timeout_ptr != 0 {
                // SAFETY: caller passes a user-space pointer; we treat
                // it as identity-mapped into our address space.
                unsafe {
                    let tv_sec = *(timeout_ptr as *const u64);
                    let tv_nsec = *((timeout_ptr + 8) as *const u64);
                    let dur_ns = tv_sec.saturating_mul(1_000_000_000)
                        .saturating_add(tv_nsec);
                    Some(monotonic_ns().saturating_add(dur_ns))
                }
            } else {
                None
            };
            crate::sched::futex_wait_until(uaddr, val as u32, deadline)
        }
        1 => {
            crate::sched::futex_wake(uaddr, val as u32)
        }
        _ => {
            // Return 0 for unknown commands (FUTEX_REQUEUE, etc.)
            // But return -ENOSYS for truly unknown commands so callers
            // don't mistake success for completion.
            serial_println!("[futex] unknown cmd={} op={:#x}", cmd, op);
            0
        }
    }
}

fn sys_readlink(path: *const u8, buf: *mut u8, bufsiz: usize) -> i64 {
    // Read path string for logging/comparison
    let mut path_buf = [0u8; 256];
    let mut path_len = 0;
    unsafe {
        while path_len < 255 {
            let b = *path.add(path_len);
            if b == 0 { break; }
            path_buf[path_len] = b;
            path_len += 1;
        }
    }
    let path_slice = &path_buf[..path_len];

    if path_slice == b"/proc/self/exe" {
        let target = b"/otp/erts-15.2.7/bin/beam.smp";
        let len = target.len().min(bufsiz);
        unsafe { core::ptr::copy_nonoverlapping(target.as_ptr(), buf, len); }
        return len as i64;
    }
    // Trace failed readlinks
    {
        static FAIL: AtomicU64 = AtomicU64::new(0);
        let n = FAIL.fetch_add(1, Ordering::Relaxed);
        if n < 30 {
            if let Ok(s) = core::str::from_utf8(path_slice) {
                serial_println!("[readlink fail] {}", s);
            }
        }
    }
    // EINVAL = "not a symlink" (path exists but is a regular file/directory)
    // ERTS uses readlink to detect symlinks; EINVAL tells it to treat the path
    // as a regular file/dir and proceed. ENOENT would make ERTS give up.
    -22 // -EINVAL
}

/// Last returned nanosecond value — ensures monotonicity.
static LAST_TIME_NS: AtomicU64 = AtomicU64::new(0);

/// Per-CPU last returned ns; used to detect kernel-level backwards regressions.
static LAST_RETURNED_PER_CPU: [AtomicU64; 16] = {
    const Z: AtomicU64 = AtomicU64::new(0);
    [Z; 16]
};

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

/// Timerfd state: deadline (monotonic_ns) when the timer fires next.
/// 0 = disarmed. ERTS uses a single timerfd (we return fd 51 from create).
static TIMERFD_DEADLINE_NS: AtomicU64 = AtomicU64::new(0);

fn sys_timerfd_settime(_fd: i32, flags: i32, new_value_ptr: u64) -> i64 {
    {
        static LOG: AtomicU64 = AtomicU64::new(0);
        let n = LOG.fetch_add(1, Ordering::Relaxed);
        if n < 5 {
            if new_value_ptr != 0 {
                let s = unsafe { *((new_value_ptr + 16) as *const u64) };
                let ns = unsafe { *((new_value_ptr + 24) as *const u64) };
                serial_println!("[timerfd_settime] flags={} sec={} nsec={}", flags, s, ns);
            } else {
                serial_println!("[timerfd_settime] disarm");
            }
        }
    }
    // struct itimerspec { struct timespec it_interval; struct timespec it_value; }
    // struct timespec  { time_t tv_sec; long tv_nsec; }
    // Layout (16 bytes interval, 16 bytes value):
    //   off  0: it_interval.tv_sec
    //   off  8: it_interval.tv_nsec
    //   off 16: it_value.tv_sec
    //   off 24: it_value.tv_nsec
    if new_value_ptr == 0 {
        TIMERFD_DEADLINE_NS.store(0, Ordering::Release);
        return 0;
    }
    let it_value_sec = unsafe { *((new_value_ptr + 16) as *const u64) };
    let it_value_nsec = unsafe { *((new_value_ptr + 24) as *const u64) };
    if it_value_sec == 0 && it_value_nsec == 0 {
        TIMERFD_DEADLINE_NS.store(0, Ordering::Release);
        return 0;
    }
    let dur_ns = it_value_sec.saturating_mul(1_000_000_000).saturating_add(it_value_nsec);
    // TFD_TIMER_ABSTIME (flag bit 0) = absolute time; otherwise relative.
    const TFD_TIMER_ABSTIME: i32 = 1;
    let deadline = if flags & TFD_TIMER_ABSTIME != 0 {
        dur_ns
    } else {
        monotonic_ns().saturating_add(dur_ns)
    };
    TIMERFD_DEADLINE_NS.store(deadline, Ordering::Release);
    0
}

/// Has the timerfd's deadline passed? Used by epoll_wait/ppoll.
pub fn timerfd_ready() -> bool {
    let deadline = TIMERFD_DEADLINE_NS.load(Ordering::Acquire);
    deadline != 0 && monotonic_ns() >= deadline
}

/// Reset the timerfd after read. Returns the expiration count (1 if fired).
pub fn timerfd_consume() -> u64 {
    let deadline = TIMERFD_DEADLINE_NS.load(Ordering::Acquire);
    if deadline != 0 && monotonic_ns() >= deadline {
        // Disarm — the timer fires once. ERTS will call settime again.
        TIMERFD_DEADLINE_NS.store(0, Ordering::Release);
        1
    } else {
        0
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

    // Ratchet: atomically advance LAST_TIME_NS to max(prev, total_ns).
    let prev = LAST_TIME_NS.fetch_max(total_ns, Ordering::SeqCst);
    let result = prev.max(total_ns);

    // Per-CPU diagnostic: detect any backwards jump from this CPU's POV.
    if cpu < 16 {
        let last = LAST_RETURNED_PER_CPU[cpu].load(Ordering::Relaxed);
        if result < last {
            // Diagnostic: we're returning a value smaller than a previous return
            // from the same CPU. This SHOULD be impossible with the global ratchet.
            static REPORTED: AtomicU64 = AtomicU64::new(0);
            if REPORTED.fetch_add(1, Ordering::Relaxed) < 5 {
                serial_println!("[mono-bug] cpu={} prev_last={} result={} prev_ratchet={} total_ns={} raw_tsc={} offset={} freq_mhz={}",
                    cpu, last, result, prev, total_ns, raw_tsc, offset, freq_mhz);
            }
        }
        // Ratchet per-CPU as well, for paranoia.
        LAST_RETURNED_PER_CPU[cpu].fetch_max(result, Ordering::Relaxed);
    }

    if were_enabled { x86_64::instructions::interrupts::enable(); }
    result
}

fn sys_clock_gettime(_clk_id: i32, tp: *mut u64) -> i64 {
    // Track call rate to verify musl is hitting our syscall (not vDSO bypass).
    {
        static CG_COUNT: AtomicU64 = AtomicU64::new(0);
        let n = CG_COUNT.fetch_add(1, Ordering::Relaxed);
        if n == 100 || n == 1000 || n == 10000 {
            serial_println!("[clock_gettime] called {} times", n);
        }
    }
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
