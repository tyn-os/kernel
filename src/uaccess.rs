//! Stage 3b.2 — the ONE audited kernel↔user memory access path (SMAP era).
//!
//! Under `CR4.SMAP` a ring-0 access to a US=1 (BEAM) page faults unless `EFLAGS.AC=1`.
//! Every legitimate kernel-touches-BEAM-memory site routes through here: validate the
//! pointer is user memory (else EFAULT — the confused-deputy guard, since SMAP forces
//! the stac but does NOT stop the kernel touching kernel memory on BEAM's behalf), open
//! the SMAP window (`stac`), touch ONLY the validated range, close it (`clac`). The
//! bounds-check and the stac/clac are never split — validation and the AC window are one
//! unit, so a validated range can never be accessed without the check, nor the check
//! passed without immediately narrowing the window.
//!
//! `with_user_access` is the default (in-place, tight lexical stac/clac window);
//! `copy_from_user`/`copy_to_user` are for genuine bulk transfers. Both share the single
//! `paging::user_accessible` validation.
//!
//! NOTE: `stac`/`clac` #UD if `CR4.SMAP=0`, so these are only ever called after SMAP is
//! enabled (right before `jump_to_user`) — i.e. only from runtime syscall handlers, never
//! during boot.

pub const EFAULT: isize = -14;

// ⚠️ INVARIANT — DO NOT add `nomem` to these asm blocks (or to any stac/clac).
// stac/clac open and close the SMAP AC window; they MUST act as compiler memory
// barriers. `nomem` tells the compiler the asm touches no memory, so it is then free to
// reorder the guarded load/store OUTSIDE the window — running it with AC=0 → SMAP fault,
// or worse, a validated access that never actually happened under AC. That is a SILENT
// guard hole in EVERY uaccess site at once (it shipped once as `nomem` and only the
// enumerator caught it: sys_writev's copy stayed a fault-site). No `nomem`, ever — the
// asm must read as memory-touching so accesses stay pinned between stac and clac. Same
// "make the trap un-reintroducible" discipline as the US-AND invariant in paging.rs.
#[inline(always)]
unsafe fn stac() {
    unsafe { core::arch::asm!("stac", options(nostack)) };
}

#[inline(always)]
unsafe fn clac() {
    unsafe { core::arch::asm!("clac", options(nostack)) };
}

/// In-place guarded access (the default): validate `[ptr, ptr+len)` is user memory, then
/// run `f` with the raw base pointer inside a tight stac/clac window. `f` MUST touch only
/// `[ptr, ptr+len)`. Returns `Err(EFAULT)` if the range isn't entirely US=1 user memory —
/// the caller returns that errno to BEAM.
///
/// # Safety
/// The kernel briefly gains access to user memory; `f` must confine itself to the
/// validated range and not call back into user-access (no nested stac).
#[inline]
pub unsafe fn with_user_access<R>(
    ptr: u64,
    len: u64,
    f: impl FnOnce(*mut u8) -> R,
) -> Result<R, isize> {
    if !crate::memory::paging::user_accessible(ptr, len) {
        return Err(EFAULT);
    }
    unsafe {
        stac();
        let r = f(ptr as *mut u8);
        clac();
        Ok(r)
    }
}

/// Bulk copy IN: user `[src, src+dst.len())` → kernel `dst`.
///
/// # Safety
/// `dst` is a valid kernel buffer; the user range is validated here.
#[inline]
pub unsafe fn copy_from_user(dst: &mut [u8], src: u64) -> Result<(), isize> {
    if !crate::memory::paging::user_accessible(src, dst.len() as u64) {
        return Err(EFAULT);
    }
    unsafe {
        stac();
        core::ptr::copy_nonoverlapping(src as *const u8, dst.as_mut_ptr(), dst.len());
        clac();
    }
    Ok(())
}

/// Guarded read of a single u32 from user memory (futex words, clone tids). An aligned
/// u32 read is atomic on x86-64, so this stands in for an atomic load of a futex word.
///
/// # Safety
/// The user address is validated here.
#[inline]
pub unsafe fn read_user_u32(addr: u64) -> Result<u32, isize> {
    if !crate::memory::paging::user_accessible(addr, 4) {
        return Err(EFAULT);
    }
    unsafe {
        stac();
        let v = core::ptr::read(addr as *const u32);
        clac();
        Ok(v)
    }
}

/// Guarded write of a single u32 to user memory.
///
/// # Safety
/// The user address is validated here.
#[inline]
pub unsafe fn write_user_u32(addr: u64, val: u32) -> Result<(), isize> {
    if !crate::memory::paging::user_accessible(addr, 4) {
        return Err(EFAULT);
    }
    unsafe {
        stac();
        core::ptr::write(addr as *mut u32, val);
        clac();
    }
    Ok(())
}

/// Guarded copy of a NUL-terminated user string into `dst`, up to `dst.len()` bytes.
/// Validates + reads one byte at a time (page-safe: a string ending just before an
/// unmapped boundary copies fine, and never over-reads), stopping at NUL or when `dst`
/// fills. Returns the length copied (excluding NUL), or EFAULT if a byte to read isn't
/// user memory. Cold-path (open/stat/unlink paths), so per-byte validation is fine.
///
/// # Safety
/// `dst` is a valid kernel buffer; the user bytes are validated here.
#[inline]
pub unsafe fn copy_user_cstr(dst: &mut [u8], src: u64) -> Result<usize, isize> {
    let mut n = 0;
    while n < dst.len() {
        let p = src + n as u64;
        if !crate::memory::paging::user_accessible(p, 1) {
            return Err(EFAULT);
        }
        unsafe {
            stac();
            let b = core::ptr::read(p as *const u8);
            clac();
            if b == 0 {
                break;
            }
            dst[n] = b;
        }
        n += 1;
    }
    Ok(n)
}

/// Bulk copy OUT: kernel `src` → user `[dst, dst+src.len())`.
///
/// # Safety
/// `src` is a valid kernel buffer; the user range is validated here.
#[inline]
pub unsafe fn copy_to_user(dst: u64, src: &[u8]) -> Result<(), isize> {
    if !crate::memory::paging::user_accessible(dst, src.len() as u64) {
        return Err(EFAULT);
    }
    unsafe {
        stac();
        core::ptr::copy_nonoverlapping(src.as_ptr(), dst as *mut u8, src.len());
        clac();
    }
    Ok(())
}
