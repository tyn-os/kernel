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

/// Next file descriptor to allocate for VFS files.
static NEXT_VFS_FD: AtomicU64 = AtomicU64::new(1000);

/// Maximum number of simultaneously open VFS files.
const MAX_OPEN: usize = 256;

/// An open VFS file — tracks position within the archive data.
struct OpenFile {
    fd: i32,
    data_offset: usize, // offset into cpio_data() where file content starts
    data_len: usize,     // file size
    pos: usize,          // current read position
}

static OPEN_FILES: spin::Mutex<[Option<OpenFile>; MAX_OPEN]> = spin::Mutex::new({
    const NONE: Option<OpenFile> = None;
    [NONE; MAX_OPEN]
});

/// Parse a cpio newc header field (fixed-width hex ASCII).
fn parse_hex(bytes: &[u8]) -> u64 {
    let mut val = 0u64;
    for &b in bytes {
        let digit = match b {
            b'0'..=b'9' => (b - b'0') as u64,
            b'a'..=b'f' => (b - b'a' + 10) as u64,
            b'A'..=b'F' => (b - b'A' + 10) as u64,
            _ => 0,
        };
        val = (val << 4) | digit;
    }
    val
}

/// Look up a file in the cpio archive by path. Returns (data_offset, data_len).
fn cpio_lookup(path: &[u8]) -> Option<(usize, usize)> {
    let data = cpio_data();
    let mut offset = 0usize;

    while offset + 110 <= data.len() {
        // Check magic "070701"
        if &data[offset..offset + 6] != b"070701" {
            break;
        }

        let filesize = parse_hex(&data[offset + 54..offset + 62]) as usize;
        let namesize = parse_hex(&data[offset + 94..offset + 102]) as usize;

        // Name starts at offset + 110, padded to 4-byte boundary
        let name_start = offset + 110;
        let name_end = name_start + namesize - 1; // exclude NUL terminator
        let data_start = (name_start + namesize + 3) & !3; // 4-byte align
        let data_end = data_start + filesize;
        let next_entry = (data_end + 3) & !3; // 4-byte align

        if name_end > data.len() || data_end > data.len() {
            break;
        }

        let entry_name = &data[name_start..name_end];

        // Check for TRAILER
        if entry_name == b"TRAILER!!!" {
            break;
        }

        // Compare with requested path (normalize leading / and ./)
        let normalized = if path.starts_with(b"/") {
            &path[1..]
        } else if path.starts_with(b"./") {
            &path[2..]
        } else {
            path
        };
        let matches = entry_name == normalized;

        if matches {
            return Some((data_start, filesize));
        }

        offset = next_entry;
    }

    None
}

/// Open a file from the VFS. Returns fd on success, -ENOENT on failure.
pub fn open(path: &[u8]) -> i64 {
    let (data_offset, data_len) = match cpio_lookup(path) {
        Some(x) => {
            if let Ok(s) = core::str::from_utf8(path) {
                serial_println!("[vfs] open {} cpio_off={:#x} ({} bytes)", s, x.0, x.1);
            }
            // After enough modules are loaded, switch from spin-yield to
            // blocking futex. The init phase loads ~80 .beam files; after
            // that, lock contention is brief and blocking is safe.
            static OPEN_COUNT: core::sync::atomic::AtomicU64 =
                core::sync::atomic::AtomicU64::new(0);
            let n = OPEN_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if n == 99999 { // disabled — blocking futex deadlocks gen_server calls
                crate::sched::enable_blocking_futex();
            }
            x
        }
        None => return -2, // -ENOENT
    };

    let fd = NEXT_VFS_FD.fetch_add(1, Ordering::Relaxed) as i32;

    let mut files = OPEN_FILES.lock();
    for slot in files.iter_mut() {
        if slot.is_none() {
            *slot = Some(OpenFile { fd, data_offset, data_len, pos: 0 });
            return fd as i64;
        }
    }

    -24 // -EMFILE
}

/// Read from an open VFS file. Returns bytes read, 0 for EOF.
pub fn read(fd: i32, buf: *mut u8, count: usize) -> i64 {
    let mut files = OPEN_FILES.lock();
    for slot in files.iter_mut() {
        if let Some(ref mut file) = slot {
            if file.fd == fd {
                let remaining = file.data_len - file.pos;
                if remaining == 0 { return 0; }
                let to_read = count.min(remaining);
                let src = &cpio_data()[file.data_offset + file.pos..];
                // SAFETY: buf is in identity-mapped user memory.
                unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), buf, to_read); }
                file.pos += to_read;
                return to_read as i64;
            }
        }
    }
    -9 // -EBADF
}

/// Read at a specific offset without changing file position (atomic pread).
pub fn pread(fd: i32, buf: *mut u8, count: usize, offset: usize) -> i64 {
    let files = OPEN_FILES.lock();
    for slot in files.iter() {
        if let Some(ref file) = slot {
            if file.fd == fd {
                if offset >= file.data_len { return 0; }
                let remaining = file.data_len - offset;
                let to_read = count.min(remaining);
                let src = &cpio_data()[file.data_offset + offset..];
                unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), buf, to_read); }
                return to_read as i64;
            }
        }
    }
    -9 // -EBADF
}

/// Get file size for fstat.
pub fn fstat_size(fd: i32) -> Option<usize> {
    let files = OPEN_FILES.lock();
    for slot in files.iter() {
        if let Some(ref file) = slot {
            if file.fd == fd { return Some(file.data_len); }
        }
    }
    None
}

/// Seek within an open VFS file. Returns new position.
pub fn lseek(fd: i32, offset: i64, whence: i32) -> i64 {
    let mut files = OPEN_FILES.lock();
    for slot in files.iter_mut() {
        if let Some(ref mut file) = slot {
            if file.fd == fd {
                let new_pos = match whence {
                    0 => offset.max(0) as usize,                              // SEEK_SET
                    1 => (file.pos as i64).saturating_add(offset).max(0) as usize, // SEEK_CUR
                    2 => (file.data_len as i64).saturating_add(offset).max(0) as usize, // SEEK_END
                    _ => return -22, // -EINVAL
                };
                file.pos = new_pos.min(file.data_len);
                return file.pos as i64;
            }
        }
    }
    -9 // -EBADF
}

/// Close a VFS file descriptor.
pub fn close(fd: i32) -> i64 {
    let mut files = OPEN_FILES.lock();
    for slot in files.iter_mut() {
        if let Some(ref file) = slot {
            if file.fd == fd { *slot = None; return 0; }
        }
    }
    0
}

/// Check if an fd belongs to the VFS.
pub fn is_vfs_fd(fd: i32) -> bool {
    fd >= 1000
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

/// Register a directory fd for a given path.
pub fn open_dir(fd: i32, path: &[u8]) {
    // SAFETY: serialized through syscall handler
    let slots = unsafe { &mut DIR_SLOTS };
    for slot in slots.iter_mut() {
        if slot.fd == -1 {
            slot.fd = fd;
            let p = if path.starts_with(b"/") { &path[1..] } else { path };
            let len = p.len().min(127);
            slot.prefix[..len].copy_from_slice(&p[..len]);
            slot.prefix_len = len;
            slot.done = false;
            return;
        }
    }
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

                    unsafe {
                        let entry = buf.add(written);
                        // d_ino (u64)
                        *(entry as *mut u64) = seen_count as u64;
                        // d_off (u64)
                        *((entry as u64 + 8) as *mut u64) = written as u64 + reclen as u64;
                        // d_reclen (u16)
                        *((entry as u64 + 16) as *mut u16) = reclen as u16;
                        // d_type (u8) — DT_DIR=4 if has slash, DT_REG=8 otherwise
                        let d_type = if child_end < rest.len() { 4u8 } else { 8u8 };
                        *((entry as u64 + 18) as *mut u8) = d_type;
                        // d_name (NUL-terminated)
                        core::ptr::copy_nonoverlapping(child.as_ptr(), entry.add(19), child.len());
                        *entry.add(19 + child.len()) = 0;
                        // Zero padding
                        for i in (19 + name_len)..reclen {
                            *entry.add(i) = 0;
                        }
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
