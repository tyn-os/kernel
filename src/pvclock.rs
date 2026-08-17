//! kvmclock (KVM pvclock) — accurate, host-drift-corrected `CLOCK_REALTIME`.
//!
//! Upgrades the wall clock from RTC-seed + PIT-calibrated-TSC extrapolation
//! (real UTC but second-resolution seed, a fixed PIT-calibration rate error, and
//! no drift correction) to the paravirtual clock the hypervisor maintains: a
//! shared page with the host's *exact* TSC→ns scaling (`mul`/`shift`) plus a
//! monotonically host-corrected `system_time`, and a wall-clock base (real UTC of
//! `system_time = 0`). Nanosecond resolution + drift correction, no NTP daemon —
//! a small driver for standardized virtual hardware.
//!
//! Step-0 measured (docs, this arc): Nitro presents STANDARD kvmclock —
//! `hypervisor_id = "KVMKVMKVM"`, CPUID `0x40000001` eax bit 3 (`CLOCKSOURCE2`,
//! the new MSR `0x4b564d01`) and bit 24 (`CLOCKSOURCE_STABLE_BIT`) set; Linux's
//! boot log confirms `kvm-clock: Using msrs 4b564d01 and 4b564d00`.
//!
//! SMP: because the STABLE bit is set (the host guarantees a synchronized,
//! stable TSC across vCPUs), a SINGLE page registered on the BSP is read
//! correctly from any CPU — provided the reader normalizes its rdtsc to the BSP
//! TSC frame the page's `tsc_timestamp` lives in. We reuse the exact per-CPU
//! offset normalization `monotonic_ns` already applies (the caller passes the
//! corrected TSC). If the STABLE bit is NOT set we refuse (fall back to RTC)
//! rather than trust a single page across unsynchronized TSCs.

use core::sync::atomic::{compiler_fence, AtomicBool, AtomicU64, Ordering};

/// KVM `pvclock_vcpu_time_info` (the system-time page). Host-mutated; we only
/// ever `read_volatile` fields via `addr_of!`, never form a `&`/`&mut`.
#[repr(C)]
struct VcpuTimeInfo {
    version: u32,
    pad0: u32,
    tsc_timestamp: u64,
    system_time: u64,
    tsc_to_system_mul: u32,
    tsc_shift: i8,
    flags: u8,
    pad: [u8; 2],
}

/// KVM `pvclock_wall_clock` (real UTC of `system_time = 0`).
#[repr(C)]
struct WallClock {
    version: u32,
    sec: u32,
    nsec: u32,
}

// The registered pages. `static mut` in identity-mapped BSS: their virtual
// address equals their guest-physical address (what the MSR wants). u64 fields
// give ≥8-byte alignment, satisfying the MSR's 4-byte requirement (bits 1:0 are
// the enable/reserved flags).
static mut SYS: VcpuTimeInfo = VcpuTimeInfo {
    version: 0, pad0: 0, tsc_timestamp: 0, system_time: 0,
    tsc_to_system_mul: 0, tsc_shift: 0, flags: 0, pad: [0; 2],
};
static mut WALL: WallClock = WallClock { version: 0, sec: 0, nsec: 0 };

static AVAILABLE: AtomicBool = AtomicBool::new(false);
static WALL_BASE_NS: AtomicU64 = AtomicU64::new(0);

const MSR_KVM_SYSTEM_TIME_NEW: u32 = 0x4b56_4d01;
const MSR_KVM_WALL_CLOCK_NEW: u32 = 0x4b56_4d00;
const PVCLOCK_TSC_STABLE_BIT: u8 = 1;
/// Bound the seqlock retry so a misbehaving host can never hang a clock read.
const SEQLOCK_MAX_SPINS: u32 = 4096;

#[inline]
fn wrmsr(msr: u32, val: u64) {
    unsafe { x86_64::registers::model_specific::Msr::new(msr).write(val) }
}

/// KVM CPUID leaf `0x40000001` eax bit 3 — `KVM_FEATURE_CLOCKSOURCE2`.
fn has_clocksource2() -> bool {
    let sig = unsafe { core::arch::x86_64::__cpuid(0x4000_0000) };
    // "KVMKVMKVM\0\0\0": ebx="KVMK"=0x4b4d564b, ecx="VMKV"=0x564b4d56, edx="M..."=0x4d
    if sig.ebx != 0x4b4d_564b || sig.ecx != 0x564b_4d56 || sig.edx != 0x0000_004d {
        return false;
    }
    let feat = unsafe { core::arch::x86_64::__cpuid(0x4000_0001) };
    feat.eax & (1 << 3) != 0
}

/// Register the kvmclock pages on the BSP and enable, once at boot. No-op (leaves
/// the RTC fallback in place) if the feature isn't present or the page isn't
/// STABLE. Call from `main.rs` after `seed_wall_clock()`.
pub fn init() {
    if !has_clocksource2() {
        crate::serial_println!("[pvclock] kvmclock CLOCKSOURCE2 absent — RTC-seed fallback");
        return;
    }
    let wall_pa = core::ptr::addr_of!(WALL) as u64;
    let sys_pa = core::ptr::addr_of!(SYS) as u64;
    wrmsr(MSR_KVM_WALL_CLOCK_NEW, wall_pa); // host fills {version, sec, nsec}
    wrmsr(MSR_KVM_SYSTEM_TIME_NEW, sys_pa | 1); // bit 0 = enable

    // Require the STABLE flag — else a single cross-CPU page can't be trusted.
    let flags = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(SYS.flags)) };
    if flags & PVCLOCK_TSC_STABLE_BIT == 0 {
        wrmsr(MSR_KVM_SYSTEM_TIME_NEW, 0); // disable
        crate::serial_println!("[pvclock] page not STABLE (flags={:#x}) — RTC-seed fallback", flags);
        return;
    }
    let base = read_wall_ns();
    WALL_BASE_NS.store(base, Ordering::SeqCst);
    AVAILABLE.store(true, Ordering::SeqCst);
    crate::serial_println!(
        "[pvclock] kvmclock enabled (STABLE) — wall base {}s UTC, ns resolution + host drift correction",
        base / 1_000_000_000
    );
}

/// pvclock TSC→ns scale: `(delta << shift) * mul >> 32` (shift<0 ⇒ right shift).
#[inline]
fn scale_delta(delta: u64, mul: u32, shift: i8) -> u64 {
    let d = if shift < 0 { delta >> (-(shift as i32)) as u32 } else { delta << (shift as u32) };
    ((d as u128 * mul as u128) >> 32) as u64
}

/// Seqlock read of the wall-clock page → UTC ns of `system_time = 0`.
fn read_wall_ns() -> u64 {
    let vp = unsafe { core::ptr::addr_of!(WALL.version) };
    let sp = unsafe { core::ptr::addr_of!(WALL.sec) };
    let np = unsafe { core::ptr::addr_of!(WALL.nsec) };
    for _ in 0..SEQLOCK_MAX_SPINS {
        let v1 = unsafe { core::ptr::read_volatile(vp) };
        if v1 & 1 != 0 { core::hint::spin_loop(); continue; }
        compiler_fence(Ordering::Acquire);
        let sec = unsafe { core::ptr::read_volatile(sp) } as u64;
        let nsec = unsafe { core::ptr::read_volatile(np) } as u64;
        compiler_fence(Ordering::Acquire);
        let v2 = unsafe { core::ptr::read_volatile(vp) };
        if v1 == v2 {
            return sec * 1_000_000_000 + nsec;
        }
    }
    0
}

/// Real UTC ns from kvmclock, or `None` if unavailable (caller falls back to the
/// RTC-seed path). `corrected_tsc` is the caller's rdtsc normalized to the BSP
/// TSC frame — the frame the page's `tsc_timestamp` is written in.
///
/// The seqlock: read `version` (retry if odd — host mid-update), read the fields,
/// re-read `version`; if it changed the host wrote mid-read, so retry. The
/// `compiler_fence`s stop the compiler hoisting the field reads outside the
/// version bracket; on x86's TSO the hardware ordering does the rest. Bounded so
/// a stuck host can't hang the read.
pub fn realtime_ns(corrected_tsc: u64) -> Option<u64> {
    if !AVAILABLE.load(Ordering::Relaxed) {
        return None;
    }
    let base = WALL_BASE_NS.load(Ordering::Relaxed);
    let vp = unsafe { core::ptr::addr_of!(SYS.version) };
    let tp = unsafe { core::ptr::addr_of!(SYS.tsc_timestamp) };
    let stp = unsafe { core::ptr::addr_of!(SYS.system_time) };
    let mp = unsafe { core::ptr::addr_of!(SYS.tsc_to_system_mul) };
    let shp = unsafe { core::ptr::addr_of!(SYS.tsc_shift) };
    for _ in 0..SEQLOCK_MAX_SPINS {
        let v1 = unsafe { core::ptr::read_volatile(vp) };
        if v1 & 1 != 0 { core::hint::spin_loop(); continue; }
        compiler_fence(Ordering::Acquire);
        let tsc_ts = unsafe { core::ptr::read_volatile(tp) };
        let sys = unsafe { core::ptr::read_volatile(stp) };
        let mul = unsafe { core::ptr::read_volatile(mp) };
        let shift = unsafe { core::ptr::read_volatile(shp) };
        compiler_fence(Ordering::Acquire);
        let v2 = unsafe { core::ptr::read_volatile(vp) };
        if v1 == v2 {
            let delta = corrected_tsc.wrapping_sub(tsc_ts);
            return Some(base + sys + scale_delta(delta, mul, shift));
        }
    }
    None
}
