//! Pure RTC/CMOS decoding — no port I/O, host-unit-testable (tests/unit/).
//!
//! `rtc.rs` reads the raw CMOS registers (two consistent snapshots) and passes
//! them here; this module turns them into a Unix timestamp, or `None` if the
//! value is implausible (so the caller keeps a safe fallback instead of seeding a
//! garbage clock). These BCD/century/leap conversions are the fiddly kind that
//! silently produce wrong dates — Verus-tractable, and the exact place to test
//! the edges. See directions/PHASE2_LAYER1_UNIT.md.

/// Raw CMOS register values (already read as two consistent snapshots).
#[derive(Clone, Copy, PartialEq)]
pub struct RawFields {
    pub sec: u8,
    pub min: u8,
    pub hour: u8,
    pub day: u8,
    pub mon: u8,
    pub year: u8,
    pub cent: u8,
}

/// Decode one CMOS field to an integer. Status-B bit 2 clear ⇒ BCD encoding.
pub fn dec_field(v: u8, is_bcd: bool) -> u64 {
    if is_bcd {
        ((v & 0x0F) as u64) + (((v >> 4) & 0x0F) as u64) * 10
    } else {
        v as u64
    }
}

pub fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Days from 1970-01-01 to `year-month-day` (proleptic Gregorian; inputs assumed
/// already range-validated by `decode`).
pub fn days_since_epoch(year: u64, month: u8, day: u8) -> u64 {
    let mut days = 0u64;
    let mut y = 1970;
    while y < year {
        days += if is_leap(y) { 366 } else { 365 };
        y += 1;
    }
    const MDAYS: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 1u8;
    while m < month {
        days += MDAYS[(m - 1) as usize];
        if m == 2 && is_leap(year) {
            days += 1;
        }
        m += 1;
    }
    days + (day as u64 - 1)
}

/// Decode raw CMOS fields + status-B into Unix seconds (UTC), or `None` if the
/// value is implausible (out-of-range field or year outside 2020..=2100).
pub fn decode(raw: &RawFields, status_b: u8) -> Option<u64> {
    let is_bcd = status_b & 0x04 == 0; // bit 2 clear ⇒ BCD
    let is_12h = status_b & 0x02 == 0; // bit 1 clear ⇒ 12-hour

    let sec = dec_field(raw.sec, is_bcd);
    let min = dec_field(raw.min, is_bcd);
    let hour = if is_12h {
        let pm = raw.hour & 0x80 != 0; // PM flag rides the raw byte, before BCD
        let h = dec_field(raw.hour & 0x7F, is_bcd);
        if pm {
            (h % 12) + 12
        } else {
            h % 12
        }
    } else {
        dec_field(raw.hour, is_bcd)
    };
    let day = dec_field(raw.day, is_bcd);
    let mon = dec_field(raw.mon, is_bcd);
    let yy = dec_field(raw.year, is_bcd);
    let cent = dec_field(raw.cent, is_bcd);
    // Use the century register if it decodes to 19..=21, else assume 20xx.
    let year = if (19..=21).contains(&cent) {
        cent * 100 + yy
    } else {
        2000 + yy
    };

    if !(1..=12).contains(&mon)
        || !(1..=31).contains(&day)
        || hour > 23
        || min > 59
        || sec > 60
        || !(2020..=2100).contains(&year)
    {
        return None;
    }
    Some(days_since_epoch(year, mon as u8, day as u8) * 86_400 + hour * 3_600 + min * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BCD_24H: u8 = 0x02; // BCD (bit2 clear) + 24-hour (bit1 set)

    fn bcd(n: u64) -> u8 {
        (((n / 10) << 4) | (n % 10)) as u8
    }
    fn raw(y2: u64, cent: u64, mon: u64, day: u64, h: u64, mi: u64, s: u64) -> RawFields {
        RawFields {
            sec: bcd(s),
            min: bcd(mi),
            hour: bcd(h),
            day: bcd(day),
            mon: bcd(mon),
            year: bcd(y2),
            cent: bcd(cent),
        }
    }

    #[test]
    fn epoch_reference_2021() {
        // 2021-01-01 00:00:00 UTC = 1609459200
        assert_eq!(decode(&raw(21, 20, 1, 1, 0, 0, 0), BCD_24H), Some(1_609_459_200));
    }

    #[test]
    fn leap_day_datetime() {
        // 2024-02-29 12:34:56 UTC = 1709210096
        assert_eq!(decode(&raw(24, 20, 2, 29, 12, 34, 56), BCD_24H), Some(1_709_210_096));
    }

    #[test]
    fn leap_year_rules() {
        assert!(is_leap(2000));
        assert!(!is_leap(1900));
        assert!(is_leap(2024));
        assert!(!is_leap(2023));
        assert!(!is_leap(2100));
    }

    #[test]
    fn feb29_differs_across_leapness() {
        // The leap-day handling must actually affect the result.
        let leap = decode(&raw(24, 20, 2, 29, 0, 0, 0), BCD_24H).unwrap();
        let non = decode(&raw(23, 20, 2, 29, 0, 0, 0), BCD_24H);
        assert_ne!(non, Some(leap));
    }

    #[test]
    fn bcd_nibble_carry() {
        // The 0x09 -> 0x10 nibble boundary (9 -> 10) is where a naive shift breaks.
        assert_eq!(dec_field(0x09, true), 9);
        assert_eq!(dec_field(0x10, true), 10);
        assert_eq!(dec_field(0x59, true), 59);
        assert_eq!(dec_field(0x23, false), 0x23); // binary mode is identity
    }

    #[test]
    fn twelve_hour_pm_and_midnight() {
        let s = 0u8; // BCD + 12-hour
        // 1 PM ⇒ 13:00
        let one_pm = RawFields { hour: 0x80 | bcd(1), ..raw(24, 20, 6, 1, 0, 0, 0) };
        assert_eq!((decode(&one_pm, s).unwrap() / 3600) % 24, 13);
        // 12 AM ⇒ 00:00
        let twelve_am = RawFields { hour: bcd(12), ..raw(24, 20, 6, 1, 0, 0, 0) };
        assert_eq!((decode(&twelve_am, s).unwrap() / 3600) % 24, 0);
    }

    #[test]
    fn out_of_range_is_rejected() {
        assert_eq!(decode(&raw(24, 20, 13, 1, 0, 0, 0), BCD_24H), None); // month 13
        assert_eq!(decode(&raw(24, 20, 1, 32, 0, 0, 0), BCD_24H), None); // day 32
        assert_eq!(decode(&raw(24, 20, 1, 1, 25, 0, 0), BCD_24H), None); // hour 25
        assert_eq!(decode(&raw(10, 20, 1, 1, 0, 0, 0), BCD_24H), None);  // year 2010 < 2020
    }

    #[test]
    fn century_register_garbage_falls_back_to_20xx() {
        // cent register decodes outside 19..=21 ⇒ assume 20xx.
        assert!(decode(&raw(24, 0, 1, 1, 0, 0, 0), BCD_24H).is_some()); // -> 2024
    }
}
