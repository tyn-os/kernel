//! In-memory, volatile writable filesystem (tmpfs) mounted at /tmp and /dev/shm.
//!
//! This is the writable-FS overlay that sits *beside* the read-only cpio VFS
//! (`src/vfs.rs`). The cpio VFS serves the OTP release (beam files, priv/,
//! static assets) and is immutable; tmpfs is the scratch space unmodified
//! BEAM/Elixir code expects for temp files: `System.tmp_dir/0`, `Plug.Upload`
//! (write-temp-then-rename), `:erlang.open_port({spawn,...})` scratch, SQLite
//! `:memory:`-adjacent temp journals, ExUnit's `tmp_dir` fixtures, etc.
//!
//! ## Scope (deliberately narrow — see docs/CAPABILITY_MAP.md row 2)
//! - **Volatile.** Everything lives in the kernel heap; a reboot loses it all.
//!   No persistence, by design.
//! - **Two mounts only:** `/tmp` and `/dev/shm`, pre-created as directories at
//!   `init()`. `/` is NOT writable; a create outside the mounts is ENOENT.
//! - **Bounded.** Total bytes are capped (`CAP`) well under the 16 MiB static
//!   heap that also feeds sockets/scheduler. Over-cap writes return a partial
//!   count or ENOSPC — never a panic, never OOM of the shared heap.
//! - **No full POSIX.** No hardlinks, symlinks, xattrs, or permission
//!   enforcement (mode bits are stored/reported but not checked). Add only what
//!   a concrete target app needs.
//!
//! ## Concurrency
//! One coarse `spin::Mutex<Option<Tmpfs>>` guards the *entire* filesystem — the
//! node tree, the byte accounting, and the open-file table. Correct-and-coarse
//! beats fast-and-racy here (see docs/FUTEX_HISTORY.md for why we distrust
//! clever locking on this kernel). Every public fn takes the lock for its whole
//! duration; none of them block, so the critical sections are short.
//!
//! ## fd model
//! tmpfs fds are allocated from a private high base (`TMPFS_FD_BASE`, above the
//! VFS 1000+ range) via a monotonic counter, and routed by *membership* in the
//! open table (`is_tmpfs_fd`) — the same discipline vfs.rs uses (never a bare
//! range check), so a closed-and-recycled number can't be misrouted.

use crate::serial_println;
use crate::tmpfs_tree::{grant_write, is_mount_path, is_under, norm, parent};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, Ordering};
use spin::Mutex;

/// Total byte cap for all tmpfs file contents. The static kernel heap is 16 MiB
/// and is shared with socket buffers and the scheduler, so tmpfs is capped at a
/// fraction of it — a runaway upload gets ENOSPC, not a heap exhaustion that
/// would wedge the network stack.
const CAP: usize = 4 * 1024 * 1024;

/// Private fd base, above the VFS range (vfs.rs: sockets 500, dirs 900, files
/// 1000+). Routed by membership (`is_tmpfs_fd`), not by range.
const TMPFS_FD_BASE: i32 = 4000;

/// Cap on simultaneously-open tmpfs fds — bounds the open table.
const MAX_OPEN: usize = 256;

// open(2) flags (x86_64 Linux ABI).
const O_WRONLY: u64 = 0o1;
const O_RDWR: u64 = 0o2;
const O_CREAT: u64 = 0o100;
const O_EXCL: u64 = 0o200;
const O_TRUNC: u64 = 0o1000;
const O_APPEND: u64 = 0o2000;
const O_DIRECTORY: u64 = 0o200000;

// errno values (returned negated).
const ENOENT: i64 = -2;
const EBADF: i64 = -9;
const EEXIST: i64 = -17;
const ENOTDIR: i64 = -20;
const EISDIR: i64 = -21;
const ENOSPC: i64 = -28;
const ENOTEMPTY: i64 = -39;
const EMFILE: i64 = -24;

/// A filesystem node — a directory or a regular file. Directories carry no
/// child list; membership is derived from path prefixes in the flat `nodes`
/// map (see `children`), which keeps rename/mkdir/rmdir trivial.
struct Node {
    is_dir: bool,
    /// File contents. Empty for directories. Grows on write, counted in `total`.
    data: Vec<u8>,
    /// Stored mode bits (permission bits only; type bits added on stat). Not
    /// enforced — kept so a chmod-then-stat round-trips and umask-sensitive code
    /// sees what it set.
    mode: u32,
}

/// An open file description.
struct OpenFd {
    fd: i32,
    /// Normalised absolute path of the node this fd refers to.
    path: Vec<u8>,
    /// Current read/write offset.
    pos: usize,
    /// O_APPEND: every write repositions to end-of-file first.
    append: bool,
    /// Whether writes are permitted (O_WRONLY / O_RDWR).
    writable: bool,
    /// True if this fd was opened on a directory (for getdents64).
    is_dir: bool,
    /// getdents64 cursor: index into the sorted child list already emitted.
    dir_pos: usize,
}

struct Tmpfs {
    nodes: BTreeMap<Vec<u8>, Node>,
    /// Sum of all file `data` lengths — the quantity capped by `CAP`.
    total: usize,
    open: Vec<OpenFd>,
}

static TMPFS: Mutex<Option<Tmpfs>> = Mutex::new(None);
static NEXT_FD: AtomicI32 = AtomicI32::new(TMPFS_FD_BASE);

/// Create the mount-point directories. Called once at boot (main.rs), after the
/// heap allocator is live and before ERTS starts.
pub fn init() {
    let mut nodes = BTreeMap::new();
    nodes.insert(b"/tmp".to_vec(), Node { is_dir: true, data: Vec::new(), mode: 0o777 });
    nodes.insert(b"/dev/shm".to_vec(), Node { is_dir: true, data: Vec::new(), mode: 0o777 });
    *TMPFS.lock() = Some(Tmpfs { nodes, total: 0, open: Vec::new() });
    serial_println!("[tmpfs] mounted /tmp and /dev/shm (cap {} KiB)", CAP / 1024);
}

/// True if `path` is inside a tmpfs mount (or is a mount root). This is the
/// routing predicate the syscall layer uses to decide "does tmpfs own this
/// path?" before falling through to the cpio VFS. (Normalisation + the
/// prefix-trap-safe membership test are the pure `tmpfs_tree` core.)
pub fn owns_path(path: &[u8]) -> bool {
    is_mount_path(&norm(path))
}

/// True if `fd` is a live tmpfs descriptor (membership, not range).
pub fn is_tmpfs_fd(fd: i32) -> bool {
    if fd < TMPFS_FD_BASE {
        return false;
    }
    let g = TMPFS.lock();
    match g.as_ref() {
        Some(fs) => fs.open.iter().any(|o| o.fd == fd),
        None => false,
    }
}

fn alloc_fd() -> i32 {
    NEXT_FD.fetch_add(1, Ordering::Relaxed)
}

/// Fill a 144-byte `struct stat` for a node. `mode` is the full mode (type +
/// perm). Returns 0 (callers already zeroed the buffer or we zero here).
///
/// # Safety
/// `buf` must point to at least 144 bytes of writable user memory, or be null.
unsafe fn write_stat(buf: *mut u8, mode: u32, size: usize) {
    if buf.is_null() {
        return;
    }
    core::ptr::write_bytes(buf, 0, 144);
    *(buf.add(24) as *mut u32) = mode; // st_mode
    *(buf.add(48) as *mut u64) = size as u64; // st_size
    *(buf.add(56) as *mut u64) = 4096; // st_blksize
    // st_nlink at offset 16 — report 1 for files, 2 for dirs (self + parent).
    *(buf.add(16) as *mut u64) = if mode & 0o040000 != 0 { 2 } else { 1 };
}

/// stat(2)/newfstatat(2) for a tmpfs path. Returns 0 and fills `buf`, or a
/// negative errno. This is what makes `System.tmp_dir/0` see `/tmp` as a
/// writable directory (the first wall: without an S_IFDIR here, Elixir's
/// `write_tmp_dir` gives up and `tmp_dir` returns nil).
///
/// # Safety
/// `buf` must be null or point to a 144-byte `struct stat`.
pub unsafe fn stat(path: &[u8], buf: *mut u8) -> i64 {
    let n = norm(path);
    let g = TMPFS.lock();
    let fs = match g.as_ref() {
        Some(f) => f,
        None => return ENOENT,
    };
    match fs.nodes.get(&n) {
        Some(node) if node.is_dir => {
            write_stat(buf, 0o040000 | (node.mode & 0o7777), 4096);
            0
        }
        Some(node) => {
            write_stat(buf, 0o100000 | (node.mode & 0o7777), node.data.len());
            0
        }
        None => ENOENT,
    }
}

/// open(2)/openat(2) for a tmpfs path. Honors O_CREAT/O_EXCL/O_TRUNC/O_APPEND
/// and the access mode. Returns an fd (>= TMPFS_FD_BASE) or a negative errno.
pub fn open(path: &[u8], flags: u64, mode: u64) -> i64 {
    let n = norm(path);
    let mut g = TMPFS.lock();
    let fs = match g.as_mut() {
        Some(f) => f,
        None => return ENOENT,
    };
    if fs.open.len() >= MAX_OPEN {
        return EMFILE;
    }
    let acc = flags & 0o3;
    let writable = acc == O_WRONLY || acc == O_RDWR;

    let exists = fs.nodes.contains_key(&n);
    if exists {
        let is_dir = fs.nodes.get(&n).unwrap().is_dir;
        if flags & O_CREAT != 0 && flags & O_EXCL != 0 {
            return EEXIST;
        }
        if is_dir {
            // Writing to a directory is EISDIR; a read/O_DIRECTORY open is fine
            // (used by readdir/getdents64).
            if writable {
                return EISDIR;
            }
            let fd = alloc_fd();
            fs.open.push(OpenFd {
                fd, path: n, pos: 0, append: false, writable: false, is_dir: true, dir_pos: 0,
            });
            return fd as i64;
        }
        // Opening an existing file. O_DIRECTORY on a non-dir is ENOTDIR.
        if flags & O_DIRECTORY != 0 {
            return ENOTDIR;
        }
        if flags & O_TRUNC != 0 && writable {
            let node = fs.nodes.get_mut(&n).unwrap();
            fs.total -= node.data.len();
            node.data.clear();
        }
        let append = flags & O_APPEND != 0;
        let pos = if append { fs.nodes.get(&n).unwrap().data.len() } else { 0 };
        let fd = alloc_fd();
        fs.open.push(OpenFd {
            fd, path: n, pos, append, writable, is_dir: false, dir_pos: 0,
        });
        fd as i64
    } else {
        if flags & O_CREAT == 0 {
            return ENOENT;
        }
        if flags & O_DIRECTORY != 0 {
            return ENOENT; // can't O_CREAT a directory via open()
        }
        // Parent must exist and be a directory.
        let par = parent(&n);
        match fs.nodes.get(&par) {
            Some(p) if p.is_dir => {}
            Some(_) => return ENOTDIR,
            None => return ENOENT,
        }
        let perm = (mode as u32) & 0o7777;
        let perm = if perm == 0 { 0o644 } else { perm };
        fs.nodes.insert(n.clone(), Node { is_dir: false, data: Vec::new(), mode: perm });
        let fd = alloc_fd();
        fs.open.push(OpenFd {
            fd, path: n, pos: 0, append: false, writable: true, is_dir: false, dir_pos: 0,
        });
        fd as i64
    }
}

/// Shared write core, used by both write(2) and pwrite(2). Writes `count` bytes
/// from `buf` at absolute offset `at` into `path`'s data, growing/zero-filling
/// as needed. Honors the byte cap: writes as much as fits and returns the
/// partial count; returns ENOSPC only if nothing at all fits. NEVER discards
/// already-written bytes (the writev lesson).
///
/// # Safety
/// `buf` must point to at least `count` readable bytes of user memory.
unsafe fn write_at(fs: &mut Tmpfs, path: &[u8], at: usize, buf: *const u8, count: usize) -> i64 {
    let node = match fs.nodes.get_mut(path) {
        Some(nd) if !nd.is_dir => nd,
        Some(_) => return EISDIR,
        None => return ENOENT,
    };
    if count == 0 {
        return 0;
    }
    // How many of `count` bytes may land under the cap — accounts for the
    // zero-filled gap of a sparse write past EOF, and treats in-place overwrite
    // as free. The arithmetic (with its off-by-one teeth) lives in the pure
    // tmpfs_tree::grant_write core; unit-tested at the cap boundary.
    let len = node.data.len();
    let n = grant_write(fs.total, CAP, at, len, count);
    if n == 0 {
        return ENOSPC; // no room (even the gap doesn't fit), nothing written
    }
    let new_end = at + n;
    if new_end > node.data.len() {
        let grow = new_end - node.data.len();
        node.data.resize(new_end, 0); // zero-fill any gap (at > len) and the tail
        fs.total += grow;
    }
    let src = core::slice::from_raw_parts(buf, n);
    node.data[at..new_end].copy_from_slice(src);
    n as i64
}

/// write(2). Advances the fd offset by the number of bytes written.
///
/// # Safety
/// `buf` must point to at least `count` readable bytes of user memory.
pub unsafe fn write(fd: i32, buf: *const u8, count: usize) -> i64 {
    let mut g = TMPFS.lock();
    let fs = match g.as_mut() {
        Some(f) => f,
        None => return EBADF,
    };
    let idx = match fs.open.iter().position(|o| o.fd == fd) {
        Some(i) => i,
        None => return EBADF,
    };
    if !fs.open[idx].writable || fs.open[idx].is_dir {
        return EBADF;
    }
    let path = fs.open[idx].path.clone();
    let at = if fs.open[idx].append {
        fs.nodes.get(&path).map(|nd| nd.data.len()).unwrap_or(0)
    } else {
        fs.open[idx].pos
    };
    let r = write_at(fs, &path, at, buf, count);
    if r > 0 {
        fs.open[idx].pos = at + r as usize;
    }
    r
}

/// pwrite64(2). Does not advance the fd offset.
///
/// # Safety
/// `buf` must point to at least `count` readable bytes of user memory.
pub unsafe fn pwrite(fd: i32, buf: *const u8, count: usize, offset: usize) -> i64 {
    let mut g = TMPFS.lock();
    let fs = match g.as_mut() {
        Some(f) => f,
        None => return EBADF,
    };
    let idx = match fs.open.iter().position(|o| o.fd == fd) {
        Some(i) => i,
        None => return EBADF,
    };
    if !fs.open[idx].writable || fs.open[idx].is_dir {
        return EBADF;
    }
    let path = fs.open[idx].path.clone();
    write_at(fs, &path, offset, buf, count)
}

/// Shared read core. Copies up to `count` bytes from `path`'s data at `at`.
///
/// # Safety
/// `buf` must point to at least `count` writable bytes of user memory.
unsafe fn read_at(node: &Node, at: usize, buf: *mut u8, count: usize) -> i64 {
    if at >= node.data.len() {
        return 0; // EOF
    }
    let n = count.min(node.data.len() - at);
    core::ptr::copy_nonoverlapping(node.data[at..].as_ptr(), buf, n);
    n as i64
}

/// read(2). Advances the fd offset.
///
/// # Safety
/// `buf` must point to at least `count` writable bytes of user memory.
pub unsafe fn read(fd: i32, buf: *mut u8, count: usize) -> i64 {
    let mut g = TMPFS.lock();
    let fs = match g.as_mut() {
        Some(f) => f,
        None => return EBADF,
    };
    let idx = match fs.open.iter().position(|o| o.fd == fd) {
        Some(i) => i,
        None => return EBADF,
    };
    if fs.open[idx].is_dir {
        return EISDIR;
    }
    let path = fs.open[idx].path.clone();
    let at = fs.open[idx].pos;
    let node = match fs.nodes.get(&path) {
        Some(nd) => nd,
        None => return EBADF,
    };
    let r = read_at(node, at, buf, count);
    if r > 0 {
        fs.open[idx].pos = at + r as usize;
    }
    r
}

/// pread64(2). Does not advance the fd offset.
///
/// # Safety
/// `buf` must point to at least `count` writable bytes of user memory.
pub unsafe fn pread(fd: i32, buf: *mut u8, count: usize, offset: usize) -> i64 {
    let g = TMPFS.lock();
    let fs = match g.as_ref() {
        Some(f) => f,
        None => return EBADF,
    };
    let o = match fs.open.iter().find(|o| o.fd == fd) {
        Some(o) => o,
        None => return EBADF,
    };
    if o.is_dir {
        return EISDIR;
    }
    let node = match fs.nodes.get(&o.path) {
        Some(nd) => nd,
        None => return EBADF,
    };
    read_at(node, offset, buf, count)
}

/// lseek(2). SEEK_SET=0, SEEK_CUR=1, SEEK_END=2. Returns the new offset.
pub fn lseek(fd: i32, offset: i64, whence: i32) -> i64 {
    let mut g = TMPFS.lock();
    let fs = match g.as_mut() {
        Some(f) => f,
        None => return EBADF,
    };
    let len = {
        let path = match fs.open.iter().find(|o| o.fd == fd) {
            Some(o) => o.path.clone(),
            None => return EBADF,
        };
        fs.nodes.get(&path).map(|nd| nd.data.len()).unwrap_or(0)
    };
    let o = match fs.open.iter_mut().find(|o| o.fd == fd) {
        Some(o) => o,
        None => return EBADF,
    };
    let base = match whence {
        0 => 0i64,
        1 => o.pos as i64,
        2 => len as i64,
        _ => return -22, // EINVAL
    };
    let np = base + offset;
    if np < 0 {
        return -22; // EINVAL
    }
    o.pos = np as usize;
    np
}

/// ftruncate(2). Grows (zero-fill) or shrinks the file to `len`, honoring the
/// cap on growth.
pub fn ftruncate(fd: i32, len: usize) -> i64 {
    let mut g = TMPFS.lock();
    let fs = match g.as_mut() {
        Some(f) => f,
        None => return EBADF,
    };
    let path = match fs.open.iter().find(|o| o.fd == fd) {
        Some(o) if !o.is_dir => o.path.clone(),
        Some(_) => return EISDIR,
        None => return EBADF,
    };
    resize_path(fs, &path, len)
}

/// truncate(2) by path.
pub fn truncate(path: &[u8], len: usize) -> i64 {
    let n = norm(path);
    let mut g = TMPFS.lock();
    let fs = match g.as_mut() {
        Some(f) => f,
        None => return ENOENT,
    };
    if !fs.nodes.contains_key(&n) {
        return ENOENT;
    }
    resize_path(fs, &n, len)
}

fn resize_path(fs: &mut Tmpfs, path: &[u8], len: usize) -> i64 {
    let cur = match fs.nodes.get(path) {
        Some(nd) if nd.is_dir => return EISDIR,
        Some(nd) => nd.data.len(),
        None => return ENOENT,
    };
    if len > cur {
        let grow = len - cur;
        if fs.total + grow > CAP {
            return ENOSPC;
        }
        fs.total += grow;
    } else {
        fs.total -= cur - len;
    }
    fs.nodes.get_mut(path).unwrap().data.resize(len, 0);
    0
}

/// fstat(2) for a tmpfs fd.
///
/// # Safety
/// `buf` must be null or point to a 144-byte `struct stat`.
pub unsafe fn fstat(fd: i32, buf: *mut u8) -> i64 {
    let g = TMPFS.lock();
    let fs = match g.as_ref() {
        Some(f) => f,
        None => return EBADF,
    };
    let o = match fs.open.iter().find(|o| o.fd == fd) {
        Some(o) => o,
        None => return EBADF,
    };
    match fs.nodes.get(&o.path) {
        Some(node) if node.is_dir => {
            write_stat(buf, 0o040000 | (node.mode & 0o7777), 4096);
            0
        }
        Some(node) => {
            write_stat(buf, 0o100000 | (node.mode & 0o7777), node.data.len());
            0
        }
        None => EBADF,
    }
}

/// close(2). Returns 0 if `fd` was a tmpfs fd (and removes it), or EBADF if not
/// — the caller (sys_close) treats EBADF as "not mine, try the next subsystem".
pub fn close(fd: i32) -> i64 {
    if fd < TMPFS_FD_BASE {
        return EBADF;
    }
    let mut g = TMPFS.lock();
    let fs = match g.as_mut() {
        Some(f) => f,
        None => return EBADF,
    };
    match fs.open.iter().position(|o| o.fd == fd) {
        Some(i) => {
            fs.open.remove(i);
            0
        }
        None => EBADF,
    }
}

/// unlink(2). Removes a regular file (frees its bytes). Refuses directories
/// (EISDIR — the caller should use rmdir).
pub fn unlink(path: &[u8]) -> i64 {
    let n = norm(path);
    let mut g = TMPFS.lock();
    let fs = match g.as_mut() {
        Some(f) => f,
        None => return ENOENT,
    };
    match fs.nodes.get(&n) {
        Some(node) if node.is_dir => EISDIR,
        Some(node) => {
            let freed = node.data.len();
            fs.nodes.remove(&n);
            fs.total -= freed;
            0
        }
        None => ENOENT,
    }
}

/// rename(2). The write-temp-then-rename pattern Plug.Upload uses. Both paths
/// must be within tmpfs (checked by the caller via owns_path). Moves a file or
/// an empty directory; overwrites an existing destination file.
pub fn rename(old: &[u8], new: &[u8]) -> i64 {
    let o = norm(old);
    let nw = norm(new);
    if o == nw {
        return 0;
    }
    let mut g = TMPFS.lock();
    let fs = match g.as_mut() {
        Some(f) => f,
        None => return ENOENT,
    };
    if !fs.nodes.contains_key(&o) {
        return ENOENT;
    }
    // Destination parent must exist and be a directory.
    let par = parent(&nw);
    match fs.nodes.get(&par) {
        Some(p) if p.is_dir => {}
        Some(_) => return ENOTDIR,
        None => return ENOENT,
    }
    let src_is_dir = fs.nodes.get(&o).unwrap().is_dir;
    // If destination exists: a file dest is overwritten; a dir dest must be
    // empty and same type.
    if let Some(dst) = fs.nodes.get(&nw) {
        if dst.is_dir != src_is_dir {
            return if dst.is_dir { EISDIR } else { ENOTDIR };
        }
        if dst.is_dir {
            if has_children(fs, &nw) {
                return ENOTEMPTY;
            }
        } else {
            let freed = dst.data.len();
            fs.total -= freed;
        }
        fs.nodes.remove(&nw);
    }
    // Directory rename with children is not supported (would need to re-key
    // every descendant); refuse it rather than silently orphan them.
    if src_is_dir && has_children(fs, &o) {
        return -22; // EINVAL — non-empty directory rename unsupported
    }
    let node = fs.nodes.remove(&o).unwrap();
    fs.nodes.insert(nw, node);
    0
}

/// True if any node is a (possibly deep) descendant of `dir` — the
/// delete-non-empty gate. The prefix-trap-safe predicate is the pure
/// `tmpfs_tree::is_under` core.
fn has_children(fs: &Tmpfs, dir: &[u8]) -> bool {
    fs.nodes.keys().any(|k| is_under(k, dir))
}

/// mkdir(2). Parent must exist and be a directory.
pub fn mkdir(path: &[u8], mode: u64) -> i64 {
    let n = norm(path);
    let mut g = TMPFS.lock();
    let fs = match g.as_mut() {
        Some(f) => f,
        None => return ENOENT,
    };
    if fs.nodes.contains_key(&n) {
        return EEXIST;
    }
    let par = parent(&n);
    match fs.nodes.get(&par) {
        Some(p) if p.is_dir => {}
        Some(_) => return ENOTDIR,
        None => return ENOENT,
    }
    let perm = (mode as u32) & 0o7777;
    let perm = if perm == 0 { 0o755 } else { perm };
    fs.nodes.insert(n, Node { is_dir: true, data: Vec::new(), mode: perm });
    0
}

/// rmdir(2). Removes an empty directory. Refuses mount roots and non-empty dirs.
pub fn rmdir(path: &[u8]) -> i64 {
    let n = norm(path);
    if n == b"/tmp" || n == b"/dev/shm" {
        return -1; // EPERM — don't let anyone unmount the roots
    }
    let mut g = TMPFS.lock();
    let fs = match g.as_mut() {
        Some(f) => f,
        None => return ENOENT,
    };
    match fs.nodes.get(&n) {
        Some(node) if !node.is_dir => return ENOTDIR,
        None => return ENOENT,
        _ => {}
    }
    if has_children(fs, &n) {
        return ENOTEMPTY;
    }
    fs.nodes.remove(&n);
    0
}

/// access(2)/faccessat(2). We don't enforce permission bits, so this is a pure
/// existence check: 0 if the path exists in tmpfs, ENOENT otherwise.
pub fn access(path: &[u8]) -> i64 {
    let n = norm(path);
    let g = TMPFS.lock();
    match g.as_ref() {
        Some(fs) if fs.nodes.contains_key(&n) => 0,
        _ => ENOENT,
    }
}

/// getdents64(3) for a tmpfs directory fd. Emits `linux_dirent64` records for
/// the directory's immediate children, resuming from the fd's `dir_pos` cursor
/// so repeated calls paginate and eventually return 0 (end of directory).
///
/// # Safety
/// `buf` must point to at least `count` writable bytes of user memory.
pub unsafe fn getdents64(fd: i32, buf: *mut u8, count: usize) -> i64 {
    let mut g = TMPFS.lock();
    let fs = match g.as_mut() {
        Some(f) => f,
        None => return EBADF,
    };
    let idx = match fs.open.iter().position(|o| o.fd == fd) {
        Some(i) => i,
        None => return EBADF,
    };
    if !fs.open[idx].is_dir {
        return ENOTDIR;
    }
    let dir = fs.open[idx].path.clone();
    let start = fs.open[idx].dir_pos;

    // Immediate children: keys with `dir/` prefix and no further '/'.
    let mut prefix = dir.clone();
    if prefix != b"/" {
        prefix.push(b'/');
    }
    let mut names: Vec<&[u8]> = Vec::new();
    for k in fs.nodes.keys() {
        if k.len() > prefix.len() && k.starts_with(&prefix) {
            let rest = &k[prefix.len()..];
            if !rest.contains(&b'/') {
                names.push(rest);
            }
        }
    }
    // BTreeMap iterates sorted, so `names` is already in a stable order.

    let mut off = 0usize; // bytes written into buf
    let mut emitted = start;
    let total = names.len();
    while emitted < total {
        let name = names[emitted];
        // linux_dirent64: d_ino(8) d_off(8) d_reclen(2) d_type(1) d_name(...\0)
        let reclen = (19 + name.len() + 1 + 7) & !7; // align up to 8
        if off + reclen > count {
            break; // no room for this entry in this call
        }
        let rec = buf.add(off);
        *(rec as *mut u64) = (emitted as u64) + 1; // d_ino (nonzero)
        *(rec.add(8) as *mut u64) = (emitted as u64) + 1; // d_off (next cursor)
        *(rec.add(16) as *mut u16) = reclen as u16; // d_reclen
        // d_type: 4=DT_DIR, 8=DT_REG. Look the child up for its type.
        let mut child = prefix.clone();
        child.extend_from_slice(name);
        let dtype = match fs.nodes.get(&child) {
            Some(nd) if nd.is_dir => 4u8,
            _ => 8u8,
        };
        *(rec.add(18) as *mut u8) = dtype;
        core::ptr::copy_nonoverlapping(name.as_ptr(), rec.add(19), name.len());
        *rec.add(19 + name.len()) = 0; // NUL
        // zero any alignment padding
        for p in (19 + name.len() + 1)..reclen {
            *rec.add(p) = 0;
        }
        off += reclen;
        emitted += 1;
    }
    fs.open[idx].dir_pos = emitted;
    off as i64
}
