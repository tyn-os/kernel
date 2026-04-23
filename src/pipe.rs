//! Minimal pipe implementation with ring buffers.
//!
//! Each pipe has a 4 KiB buffer. Writes store data, reads consume it.
//! When the buffer is empty, read blocks (yields to the other cooperative
//! thread) and retries. This matches the POSIX blocking-read semantics
//! that ERTS expects for its wakeup pipes.

use core::sync::atomic::{AtomicU64, Ordering};

const PIPE_BUF_SIZE: usize = 4096;
const MAX_PIPES: usize = 16;

struct Pipe {
    buf: [u8; PIPE_BUF_SIZE],
    head: usize, // read position
    tail: usize, // write position
    read_fd: i32,
    write_fd: i32,
    nonblock: bool, // O_NONBLOCK set via fcntl
}

impl Pipe {
    const fn empty() -> Self {
        Pipe {
            buf: [0; PIPE_BUF_SIZE],
            head: 0,
            tail: 0,
            read_fd: -1,
            write_fd: -1,
            nonblock: false,
        }
    }

    fn len(&self) -> usize {
        self.tail.wrapping_sub(self.head) % (PIPE_BUF_SIZE + 1)
    }

    fn is_empty(&self) -> bool {
        self.head == self.tail
    }

    fn write(&mut self, data: &[u8]) -> usize {
        let mut written = 0;
        for &byte in data {
            let next_tail = (self.tail + 1) % PIPE_BUF_SIZE;
            if next_tail == self.head {
                break; // buffer full
            }
            self.buf[self.tail] = byte;
            self.tail = next_tail;
            written += 1;
        }
        written
    }

    fn read(&mut self, buf: *mut u8, count: usize) -> usize {
        let mut nread = 0;
        // SAFETY: buf is identity-mapped user memory.
        unsafe {
            while nread < count && self.head != self.tail {
                *buf.add(nread) = self.buf[self.head];
                self.head = (self.head + 1) % PIPE_BUF_SIZE;
                nread += 1;
            }
        }
        nread
    }
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

    // SAFETY: Single-threaded cooperative access.
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

    // Out of pipes — still return fds but they won't work
    (read_fd, write_fd)
}

/// Write data to a pipe. Returns bytes written.
pub fn write(fd: i32, data: *const u8, count: usize) -> i64 {
    let result;
    {
        let _lock = PIPE_LOCK.lock();
        result = unsafe {
            let mut ret = -9i64; // -EBADF
            for (idx, pipe) in PIPES.iter_mut().enumerate() {
                if pipe.write_fd == fd {
                    let mut written = 0;
                    for i in 0..count {
                        let byte = *data.add(i);
                        let next_tail = (pipe.tail + 1) % PIPE_BUF_SIZE;
                        if next_tail == pipe.head {
                            break; // full
                        }
                        pipe.buf[pipe.tail] = byte;
                        pipe.tail = next_tail;
                        written += 1;
                    }
                    if fd == 205 {
                        crate::serial_println!("[pipe] write fd=205 slot={} read_fd={} head={} tail={} written={}",
                            idx, pipe.read_fd, pipe.head, pipe.tail, written);
                    }
                    ret = written as i64;
                    break;
                }
            }
            ret
        };
    } // _lock dropped
    result
}

/// Read data from a pipe. Non-blocking pipes return -EAGAIN when empty.
/// Blocking pipes yield once and return -EINTR if still empty.
pub fn read(fd: i32, buf: *mut u8, count: usize) -> i64 {
    let _lock = PIPE_LOCK.lock();
    // SAFETY: Single-threaded cooperative access.
    unsafe {
        for pipe in PIPES.iter_mut() {
            if pipe.read_fd == fd {
                // Debug: log pipe 204 state
                if fd == 204 {
                    static RD_COUNT: core::sync::atomic::AtomicUsize =
                        core::sync::atomic::AtomicUsize::new(0);
                    let c = RD_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    if c < 5 || (c > 100 && c < 105) {
                        crate::serial_println!("[pipe] read204 h={} t={} nb={}",
                            pipe.head, pipe.tail, pipe.nonblock);
                    }
                }
                // If buffer has data, return it immediately.
                if !pipe.is_empty() {
                    let n = pipe.read(buf, count) as i64;
                    // Log first few successful pipe reads
                    static COUNT: core::sync::atomic::AtomicUsize =
                        core::sync::atomic::AtomicUsize::new(0);
                    let c = COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    if c < 10 {
                        crate::serial_println!("[pipe] read fd={} got {} bytes", fd, n);
                    }
                    return n;
                }

                // Non-blocking: return EAGAIN immediately.
                if pipe.nonblock {
                    return -11; // -EAGAIN
                }

                // Blocking: yield and return -EINTR so caller retries.
                // To prevent a tight spin when this is the only runnable
                // thread on the CPU, HLT until the next timer tick first.
                drop(_lock);
                crate::sched::yield_current();
                // If yield returned immediately (no other runnable threads),
                // sleep until the next timer tick to avoid burning CPU.
                x86_64::instructions::interrupts::enable();
                x86_64::instructions::hlt();
                return -4; // -EINTR (caller retries)
            }
        }
    }
    -9 // -EBADF
}

/// Set non-blocking mode on a pipe fd.
pub fn set_nonblock(fd: i32, nonblock: bool) {
    // SAFETY: Single-threaded cooperative access.
    unsafe {
        for pipe in PIPES.iter_mut() {
            if pipe.read_fd == fd || pipe.write_fd == fd {
                pipe.nonblock = nonblock;
                return;
            }
        }
    }
}

/// Check if an fd belongs to a pipe.
pub fn is_pipe_fd(fd: i32) -> bool {
    // SAFETY: Single-threaded.
    unsafe {
        PIPES.iter().any(|p| p.read_fd == fd || p.write_fd == fd)
    }
}

/// Returns true if any pipe has unread data.
pub fn any_has_data() -> bool {
    // SAFETY: Single-threaded cooperative access.
    unsafe {
        PIPES.iter().any(|p| p.read_fd != -1 && !p.is_empty())
    }
}

/// Returns true if the given fd is a pipe read-end with pending data.
pub fn has_data(fd: i32) -> bool {
    // SAFETY: Single-threaded cooperative access.
    unsafe {
        PIPES.iter().any(|p| p.read_fd == fd && !p.is_empty())
    }
}

/// Close a pipe fd.
pub fn close(fd: i32) {
    let _lock = PIPE_LOCK.lock();
    // SAFETY: Single-threaded.
    unsafe {
        for pipe in PIPES.iter_mut() {
            if pipe.read_fd == fd {
                pipe.read_fd = -1;
            }
            if pipe.write_fd == fd {
                pipe.write_fd = -1;
            }
            // If both ends closed, free the slot
            if pipe.read_fd == -1 && pipe.write_fd == -1 {
                *pipe = Pipe::empty();
            }
        }
    }
}
