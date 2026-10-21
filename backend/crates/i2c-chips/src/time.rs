//! Civil dates without std: `Date.UTC` and `toISOString()` for RTC readings and the
//! server's ISO timestamps.

use alloc::format;
use alloc::string::String;

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's days_from_civil).
/// Months outside 1..12 and days past the month's end roll over, as `Date.UTC` does.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let (y, m) = (y + (m - 1).div_euclid(12), (m - 1).rem_euclid(12) + 1);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468 + (d - 1)
}

/// `Date.UTC(y, month0, d, h, mi, s)` in ms.
pub fn utc_ms(y: i64, month0: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
    (days_from_civil(y, month0 + 1, d) * 86_400 + h * 3600 + mi * 60 + s) * 1000
}

/// (year, month 1..12, day) for days since the epoch (civil_from_days).
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

/// `new Date(ms).toISOString()`: "2026-10-17T13:45:30.000Z".
pub fn iso(ms: i64) -> String {
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let (y, m, d) = civil_from_days(days);
    let s = rem / 1000;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z", s / 3600, s % 3600 / 60, s % 60, rem % 1000)
}

/// `toISOString().replace("T", " ").slice(0, 19)`: "2026-10-17 13:45:30".
pub fn iso_seconds(ms: i64) -> String {
    let mut s = iso(ms);
    s.truncate(19);
    s.replacen('T', " ", 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_round_trip() {
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        let ms = utc_ms(2026, 9, 17, 13, 45, 30);
        assert_eq!(iso(ms), "2026-10-17T13:45:30.000Z");
        assert_eq!(iso_seconds(ms), "2026-10-17 13:45:30");
        // Day 31 of a 30-day month rolls over like Date.UTC.
        assert_eq!(iso(utc_ms(2026, 3, 31, 0, 0, 0)), "2026-05-01T00:00:00.000Z");
        assert_eq!(iso(951_782_400_123), "2000-02-29T00:00:00.123Z");
    }
}
