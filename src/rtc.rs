//! CMOS/RTC read — seed real wall-clock time at boot.
//!
//! Tyn's `CLOCK_REALTIME` is otherwise TSC-since-boot (1970 + uptime). This
//! module reads the battery-backed RTC once at startup so `syscall.rs` can serve
//! a real UTC wall clock (see `seed_wall_clock`). Second-resolution only; it
//! drifts with the TSC over long uptimes — fine for `DateTime.utc_now`, log
//! timestamps, and TLS cert-date checks. kvmclock (paravirt) is the documented
//! precision follow-on (`docs/WALL_CLOCK.md`), not built here.
//!
//! ## Target assumptions (confirmed on QEMU + Nitro/KVM)
//! - The RTC presents **UTC** (not localtime) — both QEMU (`-rtc base=utc`, the
//!   default the OVMF/SeaBIOS path uses) and Nitro/KVM. A localtime RTC would
//!   seed a skewed clock; if a future target used localtime this is where it'd
//!   need a tz offset.
//! - **24-hour** mode (status register B bit 1 set) — near-universal on these
//!   targets; the 12-hour path is handled anyway.
//! - **BCD** encoding (status register B bit 2 clear) — the usual BIOS default;
//!   the binary path is handled anyway.

use x86_64::instructions::port::Port;

const CMOS_ADDR: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

/// Read a CMOS register. Leaves the NMI-disable bit (0x80) clear.
///
/// # Safety
/// Direct port I/O; caller runs at boot with interrupts effectively quiescent.
unsafe fn cmos_read(reg: u8) -> u8 {
    Port::<u8>::new(CMOS_ADDR).write(reg);
    Port::<u8>::new(CMOS_DATA).read()
}

/// Status register A, bit 7: an RTC update is in progress (registers unstable).
fn update_in_progress() -> bool {
    unsafe { cmos_read(0x0A) & 0x80 != 0 }
}

#[derive(PartialEq, Clone, Copy)]
struct RawTime {
    sec: u8,
    min: u8,
    hour: u8,
    day: u8,
    mon: u8,
    year: u8,
    cent: u8,
}

/// # Safety: port I/O.
unsafe fn read_raw() -> RawTime {
    RawTime {
        sec: cmos_read(0x00),
        min: cmos_read(0x02),
        hour: cmos_read(0x04),
        day: cmos_read(0x07),
        mon: cmos_read(0x08),
        year: cmos_read(0x09),
        cent: cmos_read(0x32), // century register — present on most modern chipsets
    }
}

/// Read the RTC and return Unix seconds (UTC), or `None` if the value is
/// obviously implausible (so the caller keeps the safe 1970+uptime fallback
/// rather than seeding a garbage clock).
pub fn read_rtc_unix_secs() -> Option<u64> {
    // Correctness step: never read mid-update. Wait for UIP to clear, then read,
    // and re-read until two consecutive reads are identical — this defends
    // against a rollover landing between our per-register reads.
    let mut last = unsafe {
        while update_in_progress() {}
        read_raw()
    };
    for _ in 0..10 {
        unsafe {
            while update_in_progress() {}
            let cur = read_raw();
            if cur == last {
                break;
            }
            last = cur;
        }
    }

    let status_b = unsafe { cmos_read(0x0B) };
    // The pure decode (BCD/century/leap/range-validation) lives in
    // `crate::rtc_pure`, which is host-unit-tested (tests/unit/). This layer only
    // does the port I/O read above.
    let raw = crate::rtc_pure::RawFields {
        sec: last.sec,
        min: last.min,
        hour: last.hour,
        day: last.day,
        mon: last.mon,
        year: last.year,
        cent: last.cent,
    };
    crate::rtc_pure::decode(&raw, status_b)
}
