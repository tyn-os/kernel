//! In-memory VFS backed by a cpio "newc" archive embedded in the kernel.
//!
//! Supports open, read, fstat, close — enough for ERTS to load .beam files
//! and start.boot from an embedded OTP root filesystem.

use crate::serial_println;
use core::sync::atomic::{AtomicU64, Ordering};

/// Embedded cpio archive (newc format). Initially points to .rodata,
/// relocated to a safe address before ELF loading overwrites .rodata.
static CPIO_EMBEDDED: &[u8] = include_bytes!("otp-rootfs.cpio");
static mut CPIO_PTR: *const u8 = core::ptr::null();
static mut CPIO_LEN: usize = 0;

/// Get the cpio data slice (from relocated or original location).
fn cpio_data() -> &'static [u8] {
    // SAFETY: After init(), CPIO_PTR/CPIO_LEN are set.
    unsafe {
        if CPIO_PTR.is_null() {
            CPIO_EMBEDDED
        } else {
            core::slice::from_raw_parts(CPIO_PTR, CPIO_LEN)
        }
    }
}

/// Size of the embedded cpio archive, for comparing against a GRUB
/// multiboot module of the same file (Track 1 Phase 1a validation).
pub fn embedded_len() -> usize {
    CPIO_EMBEDDED.len()
}

/// Copy the cpio archive to a safe address above the kernel, so it
/// survives ELF loading which overwrites .rodata.
///
/// # Safety
/// `dest` must be a valid, writable, identity-mapped address with enough
/// space for the cpio data.
pub unsafe fn relocate(dest: usize) {
    let src = CPIO_EMBEDDED;
    core::ptr::copy_nonoverlapping(src.as_ptr(), dest as *mut u8, src.len());
    CPIO_PTR = dest as *const u8;
    CPIO_LEN = src.len();
}

/// Point the VFS at a cpio already resident at `src..src+len` (a GRUB module),
/// copying it up to `dest` (CPIO_HOME) exactly as the embedded path does. The
/// caller zeros the `src` staging area afterward.
///
/// # Safety
/// `src` and `dest` must be valid, identity-mapped, non-overlapping ranges of
/// at least `len` bytes.
pub unsafe fn relocate_from(src: usize, len: usize, dest: usize) {
    core::ptr::copy_nonoverlapping(src as *const u8, dest as *mut u8, len);
    CPIO_PTR = dest as *const u8;
    CPIO_LEN = len;
}

/// True if `path` exists in the current cpio source. Used to prove which
/// source (module vs embedded) is live via a sentinel file.
pub fn exists(path: &[u8]) -> bool {
    cpio_lookup(path).is_some()
}

/// Next file descriptor to allocate for VFS files.
static NEXT_VFS_FD: AtomicU64 = AtomicU64::new(1000);

/// Maximum number of simultaneously open VFS files.
const MAX_OPEN: usize = 256;

// The open-file table's slot type and the pure allocate/find/free logic live in
// the host-unit-tested `crate::fd_table` module (see tests/unit/).
use crate::fd_table::{self, OpenFile};

static OPEN_FILES: spin::Mutex<[Option<OpenFile>; MAX_OPEN]> = spin::Mutex::new({
    const NONE: Option<OpenFile> = None;
    [NONE; MAX_OPEN]
});

// The newc header-field parser lives in the pure, host-unit-tested `crate::cpio`
// module (see tests/unit/). The other cpio scans below reuse it.
use crate::cpio::parse_hex;

/// Look up a file in the embedded cpio archive by path. Returns (data_offset,
/// data_len). The newc parsing lives in `crate::cpio::lookup` — bounds- and
/// overflow-checked on malformed input; this just supplies the embedded bytes.
fn cpio_lookup(path: &[u8]) -> Option<(usize, usize)> {
    crate::cpio::lookup(cpio_data(), path)
}

/// Open a file from the VFS. Returns fd on success, -ENOENT on failure.
pub fn open(path: &[u8]) -> i64 {
    let (data_offset, data_len) = match cpio_lookup(path) {
        Some(x) => {
            if let Ok(s) = core::str::from_utf8(path) {
                crate::vdbg!("[vfs] open {} cpio_off={:#x} ({} bytes)", s, x.0, x.1);
            }
            // (The futex-blocking valve is re-armed on the boot harness's
            // `serial_shell ready` marker, not on a module-open count — see
            // sched.rs FUTEX_BLOCKING, syscall.rs, and docs/FUTEX_HISTORY.md.
            // Open-count proved to fire before the init deadlock and
            // reintroduced the stall.)
            x
        }
        None => return -2, // -ENOENT
    };

    let fd = NEXT_VFS_FD.fetch_add(1, Ordering::Relaxed) as i32;

    let mut files = OPEN_FILES.lock();
    match fd_table::alloc(&mut files[..], OpenFile { fd, data_offset, data_len, pos: 0 }) {
        Some(_) => fd as i64,
        None => -24, // -EMFILE
    }
}

/// Duplicate an open VFS fd (dup(2)). Returns a new fd referring to the same
/// cpio file with an independent (copied) position. ERTS's inet-driver sendfile
/// path dups the file fd before transferring and closes the dup afterward
/// (inet_drv.c: `dup_file_fd = dup(raw_file_fd)`), so without this, `sendfile`
/// receives a bogus fd and fails — which is why static assets 500'd.
pub fn dup(oldfd: i32) -> i64 {
    let mut files = OPEN_FILES.lock();
    // Copy the source file's backing info before we mutate the table for a slot.
    let (data_offset, data_len, pos) = match fd_table::find(&files[..], oldfd) {
        Some(i) => {
            let f = files[i].as_ref().unwrap();
            (f.data_offset, f.data_len, f.pos)
        }
        None => return -9, // -EBADF — not an open VFS fd
    };
    let fd = NEXT_VFS_FD.fetch_add(1, Ordering::Relaxed) as i32;
    match fd_table::alloc(&mut files[..], OpenFile { fd, data_offset, data_len, pos }) {
        Some(_) => fd as i64,
        None => -24, // -EMFILE
    }
}

/// Read from an open VFS file. Returns bytes read, 0 for EOF.
pub fn read(fd: i32, buf: *mut u8, count: usize) -> i64 {
    let mut files = OPEN_FILES.lock();
    let i = match fd_table::find(&files[..], fd) {
        Some(i) => i,
        None => return -9, // -EBADF
    };
    let file = files[i].as_mut().unwrap();
    let remaining = file.data_len - file.pos;
    if remaining == 0 { return 0; }
    let to_read = count.min(remaining);
    let src = &cpio_data()[file.data_offset + file.pos..];
    // 3b.2: cpio data (kernel) is the source; `buf` is a USER dest — guard the dest write.
    if unsafe { crate::uaccess::copy_to_user(buf as u64, &src[..to_read]) }.is_err() {
        return crate::uaccess::EFAULT as i64;
    }
    file.pos += to_read;
    to_read as i64
}

/// Read at a specific offset without changing file position (atomic pread).
pub fn pread(fd: i32, buf: *mut u8, count: usize, offset: usize) -> i64 {
    let files = OPEN_FILES.lock();
    let i = match fd_table::find(&files[..], fd) {
        Some(i) => i,
        None => return -9, // -EBADF
    };
    let file = files[i].as_ref().unwrap();
    if offset >= file.data_len { return 0; }
    let remaining = file.data_len - offset;
    let to_read = count.min(remaining);
    let src = &cpio_data()[file.data_offset + offset..];
    unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), buf, to_read); }
    to_read as i64
}

/// Get file size for fstat.
pub fn fstat_size(fd: i32) -> Option<usize> {
    let files = OPEN_FILES.lock();
    fd_table::find(&files[..], fd).map(|i| files[i].as_ref().unwrap().data_len)
}

/// Seek within an open VFS file. Returns new position.
pub fn lseek(fd: i32, offset: i64, whence: i32) -> i64 {
    let mut files = OPEN_FILES.lock();
    let i = match fd_table::find(&files[..], fd) {
        Some(i) => i,
        None => return -9, // -EBADF
    };
    let file = files[i].as_mut().unwrap();
    let new_pos = match whence {
        0 => offset.max(0) as usize,                                        // SEEK_SET
        1 => (file.pos as i64).saturating_add(offset).max(0) as usize,      // SEEK_CUR
        2 => (file.data_len as i64).saturating_add(offset).max(0) as usize, // SEEK_END
        _ => return -22, // -EINVAL
    };
    file.pos = new_pos.min(file.data_len);
    file.pos as i64
}

/// Close a VFS file descriptor.
pub fn close(fd: i32) -> i64 {
    let mut files = OPEN_FILES.lock();
    fd_table::free(&mut files[..], fd); // close of an unknown fd is a benign no-op
    0
}

/// Check if an fd belongs to the VFS.
///
/// Looks up the actual OPEN_FILES table instead of using the old
/// `fd >= 1000` heuristic — that heuristic mis-routed socket fds to
/// VFS once the monotonic socket allocator (SOCK_FD_BASE = 500)
/// wrapped past 1000 after ~500 accepts, silently dropping reads on
/// those connections. See directions/FD_TABLE.md for the stress-test
/// wall this caused.
pub fn is_vfs_fd(fd: i32) -> bool {
    let files = OPEN_FILES.lock();
    fd_table::find(&files[..], fd).is_some()
}

/// Initialize and log archive stats.
pub fn init() {
    let mut count = 0;
    let mut offset = 0usize;
    let data = cpio_data();

    while offset + 110 <= data.len() {
        if &data[offset..offset + 6] != b"070701" {
            break;
        }
        let filesize = parse_hex(&data[offset + 54..offset + 62]) as usize;
        let namesize = parse_hex(&data[offset + 94..offset + 102]) as usize;
        let name_start = offset + 110;
        let name_end = name_start + namesize - 1;
        if name_end > data.len() { break; }
        let name = &data[name_start..name_end];
        if name == b"TRAILER!!!" { break; }
        count += 1;
        let data_start = (name_start + namesize + 3) & !3;
        let data_end = data_start + filesize;
        offset = (data_end + 3) & !3;
    }

    serial_println!("[vfs] cpio: {} files, {} bytes", count, data.len());
}

// --- Directory listing support ---

/// Open directory slots. Stores the directory path prefix for getdents64.
const MAX_DIRS: usize = 8;
struct DirSlot {
    fd: i32,
    prefix: [u8; 128],
    prefix_len: usize,
    done: bool, // already returned entries
}
// SAFETY: DIR_SLOTS mutated only in open_dir/getdents64 which are serialized
// through the syscall handler. Reads are safe on x86 TSO.
static mut DIR_SLOTS: [DirSlot; MAX_DIRS] = {
    const EMPTY: DirSlot = DirSlot { fd: -1, prefix: [0; 128], prefix_len: 0, done: false };
    [EMPTY; MAX_DIRS]
};

/// Check if a path is a directory prefix in the cpio archive.
pub fn is_dir_prefix(path: &[u8]) -> bool {
    // Strip leading /
    let p = if path.starts_with(b"/") { &path[1..] } else { path };
    // Ensure it ends with / for prefix matching
    let mut prefix = [0u8; 128];
    let mut plen = p.len();
    if plen >= 127 { return false; }
    prefix[..plen].copy_from_slice(p);
    if !p.ends_with(b"/") {
        prefix[plen] = b'/';
        plen += 1;
    }

    // Scan cpio for any file starting with this prefix
    let data = cpio_data();
    let mut offset = 0;
    while offset + 110 < data.len() {
        if &data[offset..offset+6] != b"070701" { break; }
        let namesize = parse_hex(&data[offset+94..offset+102]) as usize;
        let name_start = offset + 110;
        let name_end = name_start + namesize - 1;
        if name_end > data.len() { break; }
        let name = &data[name_start..name_end];
        if name == b"TRAILER!!!" { break; }
        if name.len() >= plen && &name[..plen] == &prefix[..plen] {
            return true;
        }
        let filesize = parse_hex(&data[offset+54..offset+62]) as usize;
        let data_start = (name_start + namesize + 3) & !3;
        let data_end = data_start + filesize;
        offset = (data_end + 3) & !3;
    }
    false
}

/// Allocate a directory fd for a path that matched `is_dir_prefix`.
/// Returns the fd on success, -EMFILE if all DIR_SLOTS are in use.
///
/// The fd number is derived from the slot index (`DIR_FD_BASE + idx`) so
/// closing a slot makes its fd immediately reusable — previously this
/// took a caller-allocated fd from a monotonic counter that never
/// decremented, and DIR_SLOTS itself never freed entries, so after at
/// most MAX_DIRS opendir/closedir cycles the loader was wedged.
pub fn open_dir(path: &[u8]) -> i64 {
    let slots = unsafe { &mut DIR_SLOTS };
    for (i, slot) in slots.iter_mut().enumerate() {
        if slot.fd == -1 {
            let fd = DIR_FD_BASE + i as i32;
            slot.fd = fd;
            let p = if path.starts_with(b"/") { &path[1..] } else { path };
            let len = p.len().min(127);
            slot.prefix[..len].copy_from_slice(&p[..len]);
            slot.prefix_len = len;
            slot.done = false;
            return fd as i64;
        }
    }
    -24 // -EMFILE
}

/// Reserved fd range for VFS directory handles: 900..900+MAX_DIRS.
/// Stays under the 1000 floor used by `NEXT_VFS_FD` for file fds.
pub const DIR_FD_BASE: i32 = 900;

/// Free a dir fd's slot. Returns true if the fd was a known dir handle.
pub fn close_dir(fd: i32) -> bool {
    let slots = unsafe { &mut DIR_SLOTS };
    for slot in slots.iter_mut() {
        if slot.fd == fd {
            slot.fd = -1;
            slot.prefix_len = 0;
            slot.done = false;
            return true;
        }
    }
    false
}

/// Is this fd a live VFS directory handle?
pub fn is_dir_fd(fd: i32) -> bool {
    let slots = unsafe { &DIR_SLOTS };
    slots.iter().any(|s| s.fd == fd)
}

/// Return directory entries for a directory fd.
/// struct linux_dirent64 { u64 d_ino; u64 d_off; u16 d_reclen; u8 d_type; char d_name[]; }
pub fn getdents64(fd: i32, buf: *mut u8, count: usize) -> i64 {
    let slots = unsafe { &mut DIR_SLOTS };
    for slot in slots.iter_mut() {
        if slot.fd == fd {
            if slot.done { return 0; }
            slot.done = true;
            let mut prefix = [0u8; 129];
            let plen = slot.prefix_len;
            prefix[..plen].copy_from_slice(&slot.prefix[..plen]);
            if plen > 0 && prefix[plen-1] != b'/' {
                prefix[plen] = b'/';
                let plen = plen + 1;
                return fill_dir_entries(buf, count, &prefix[..plen]);
            }
            return fill_dir_entries(buf, count, &prefix[..plen]);
        }
    }
    0
}

/// Scan cpio for entries under the given prefix, return unique immediate children.
fn fill_dir_entries(buf: *mut u8, count: usize, prefix: &[u8]) -> i64 {
    let data = cpio_data();
    let mut offset_cpio = 0;
    let mut written = 0usize;
    let mut seen = [[0u8; 64]; 32];
    let mut seen_count = 0;

    while offset_cpio + 110 < data.len() {
        if &data[offset_cpio..offset_cpio+6] != b"070701" { break; }
        let namesize = parse_hex(&data[offset_cpio+94..offset_cpio+102]) as usize;
        let filesize = parse_hex(&data[offset_cpio+54..offset_cpio+62]) as usize;
        let name_start = offset_cpio + 110;
        let name_end = name_start + namesize - 1;
        if name_end > data.len() { break; }
        let name = &data[name_start..name_end];
        if name == b"TRAILER!!!" { break; }

        // Check if this entry is under the prefix
        if name.len() > prefix.len() && &name[..prefix.len()] == prefix {
            // Get the immediate child name (up to next /)
            let rest = &name[prefix.len()..];
            let child_end = rest.iter().position(|&b| b == b'/').unwrap_or(rest.len());
            let child = &rest[..child_end];

            if child.len() > 0 && child.len() < 64 {
                // Check if already seen
                let mut dup = false;
                for i in 0..seen_count {
                    if &seen[i][..child.len()] == child && seen[i][child.len()] == 0 {
                        dup = true;
                        break;
                    }
                }
                if !dup && seen_count < 32 {
                    seen[seen_count] = [0; 64];
                    seen[seen_count][..child.len()].copy_from_slice(child);
                    seen_count += 1;

                    // Write a linux_dirent64 entry
                    let name_len = child.len() + 1; // include NUL
                    let reclen = (19 + name_len + 7) & !7; // align to 8
                    if written + reclen > count { break; }

                    // 3b.2: build the linux_dirent64 in a kernel buffer, then ONE guarded
                    // copy to the user `buf` at `written`. reclen ≤ 19+65+7 (child<64) < 96.
                    let mut ent = [0u8; 96];
                    ent[0..8].copy_from_slice(&(seen_count as u64).to_ne_bytes()); // d_ino
                    ent[8..16].copy_from_slice(&(written as u64 + reclen as u64).to_ne_bytes()); // d_off
                    ent[16..18].copy_from_slice(&(reclen as u16).to_ne_bytes()); // d_reclen
                    ent[18] = if child_end < rest.len() { 4u8 } else { 8u8 }; // d_type: DIR/REG
                    ent[19..19 + child.len()].copy_from_slice(child); // d_name (NUL + pad already 0)
                    let _ = name_len; // reclen already accounts for the NUL
                    if unsafe { crate::uaccess::copy_to_user(buf as u64 + written as u64, &ent[..reclen]) }.is_err() {
                        return crate::uaccess::EFAULT as i64;
                    }
                    written += reclen;
                }
            }
        }

        let data_start = (name_start + namesize + 3) & !3;
        let data_end = data_start + filesize;
        offset_cpio = (data_end + 3) & !3;
    }

    written as i64
}
