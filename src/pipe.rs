//! Minimal pipe implementation with ring buffers.
//!
//! Mutation (create/write/close) is synchronized via PIPES spinlock.
//! Read-only queries (is_pipe_fd, has_data) use volatile reads for
//! performance — safe because fd/head/tail are word-sized and reads
//! see a consistent snapshot on x86 TSO.

use core::sync::atomic::{AtomicU64, Ordering};

const PIPE_BUF_SIZE: usize = 4096;
const MAX_PIPES: usize = 16;

struct Pipe {
    buf: [u8; PIPE_BUF_SIZE],
    head: usize,
    tail: usize,
    read_fd: i32,
    write_fd: i32,
    nonblock: bool,
}

impl Pipe {
    const fn empty() -> Self {
        Pipe { buf: [0; PIPE_BUF_SIZE], head: 0, tail: 0, read_fd: -1, write_fd: -1, nonblock: false }
    }
    fn is_empty(&self) -> bool { self.head == self.tail }
}

static NEXT_PIPE_FD: AtomicU64 = AtomicU64::new(200);
static mut PIPES: [Pipe; MAX_PIPES] = {
    const EMPTY: Pipe = Pipe::empty();
    [EMPTY; MAX_PIPES]
};
static PIPE_LOCK: spin::Mutex<()> = spin::Mutex::new(());

/// Create a new pipe. Returns (read_fd, write_fd).
pub fn create() -> (i32, i32) {
    let _lock = PIPE_LOCK.lock();
    let read_fd = NEXT_PIPE_FD.fetch_add(1, Ordering::Relaxed) as i32;
    let write_fd = NEXT_PIPE_FD.fetch_add(1, Ordering::Relaxed) as i32;
    unsafe {
        for pipe in PIPES.iter_mut() {
            if pipe.read_fd == -1 {
                *pipe = Pipe::empty();
                pipe.read_fd = read_fd;
                pipe.write_fd = write_fd;
                return (read_fd, write_fd);
            }
        }
    }
    (read_fd, write_fd)
}

/// Write data to a pipe. Returns bytes written.
pub fn write(fd: i32, data: *const u8, count: usize) -> i64 {
    let _lock = PIPE_LOCK.lock();
    unsafe {
        for (idx, pipe) in PIPES.iter_mut().enumerate() {
            if pipe.write_fd == fd {
                let mut written = 0;
                for i in 0..count {
                    let byte = *data.add(i);
                    let next_tail = (pipe.tail + 1) % PIPE_BUF_SIZE;
                    if next_tail == pipe.head { break; }
                    pipe.buf[pipe.tail] = byte;
                    pipe.tail = next_tail;
                    written += 1;
                }
                if fd == 205 {
                    crate::serial_println!("[pipe] write fd=205 slot={} read_fd={} head={} tail={} written={}",
                        idx, pipe.read_fd, pipe.head, pipe.tail, written);
                }
                return written as i64;
            }
        }
    }
    -9 // -EBADF
}

/// Read data from a pipe.
pub fn read(fd: i32, buf: *mut u8, count: usize) -> i64 {
    let _lock = PIPE_LOCK.lock();
    unsafe {
        for pipe in PIPES.iter_mut() {
            if pipe.read_fd == fd {
                if fd == 204 {
                    static RD: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
                    let c = RD.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    if c < 5 || (c > 100 && c < 105) {
                        crate::serial_println!("[pipe] read204 h={} t={} nb={}", pipe.head, pipe.tail, pipe.nonblock);
                    }
                }
                if !pipe.is_empty() {
                    let mut nread = 0;
                    while nread < count && pipe.head != pipe.tail {
                        *buf.add(nread) = pipe.buf[pipe.head];
                        pipe.head = (pipe.head + 1) % PIPE_BUF_SIZE;
                        nread += 1;
                    }
                    static LOG: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
                    let c = LOG.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    if c < 10 { crate::serial_println!("[pipe] read fd={} got {} bytes", fd, nread); }
                    return nread as i64;
                }
                // Write end closed → EOF
                if pipe.write_fd == -1 { return 0; }
                if pipe.nonblock { return -11; }
                drop(_lock);
                crate::sched::yield_current();
                return -4; // -EINTR — let caller retry
            }
        }
    }
    -9 // -EBADF
}

/// Set non-blocking mode on a pipe fd.
pub fn set_nonblock(fd: i32, nonblock: bool) {
    let _lock = PIPE_LOCK.lock();
    unsafe {
        for pipe in PIPES.iter_mut() {
            if pipe.read_fd == fd || pipe.write_fd == fd { pipe.nonblock = nonblock; return; }
        }
    }
}

/// Check if an fd belongs to a pipe. Lock-free read (x86 TSO safe).
pub fn is_pipe_fd(fd: i32) -> bool {
    // SAFETY: read_fd/write_fd are i32, atomic on x86. Stale reads are benign.
    unsafe { PIPES.iter().any(|p| p.read_fd == fd || p.write_fd == fd) }
}

/// Returns true if any pipe has unread data.
pub fn any_has_data() -> bool {
    unsafe { PIPES.iter().any(|p| p.read_fd != -1 && !p.is_empty()) }
}

/// Returns true if the given fd is a pipe read-end with pending data.
pub fn has_data(fd: i32) -> bool {
    unsafe { PIPES.iter().any(|p| p.read_fd == fd && !p.is_empty()) }
}

/// Close a pipe fd.
/// Write-end closes are silently ignored — keeps the pipe alive so reads
/// don't return EOF. This prevents the forker driver from crashing when
/// the fake fork's write end is "closed" by the parent.
pub fn close(fd: i32) {
    let _lock = PIPE_LOCK.lock();
    unsafe {
        for pipe in PIPES.iter_mut() {
            if pipe.read_fd == fd {
                pipe.read_fd = -1;
                if pipe.write_fd == -1 { *pipe = Pipe::empty(); }
            }
            // Don't close write ends — keeps pipe alive for forker stub
        }
    }
}
