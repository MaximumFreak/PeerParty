//! Time as the stand-ins' services write it.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since 1970 (0 before it).
pub fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Milliseconds since 1970 (0 before it).
pub fn millis(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// (year, month, day) of a day counted from 1970-01-01 (Howard Hinnant's
/// days_from_civil, inverted).
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// UTC time as ISO 8601 with milliseconds (`2026-10-30T09:15:00.000Z`), the way PlayFab,
/// Unity's services and Xbox Live write it.
pub fn iso(t: SystemTime) -> String {
    let ms = millis(t);
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (year, month, day) = civil(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60,
        ms.rem_euclid(1000)
    )
}

/// UTC time as C's `ctime` writes it, without the newline (`Mon Nov 23 02:09:18 2026`),
/// the way Steam's Web API tells its server time.
pub fn ctime(secs: i64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let secs = secs.max(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (year, month, day) = civil(days);
    // 1970-01-01 was a Thursday.
    format!(
        "{} {} {day:2} {:02}:{:02}:{:02} {year}",
        DAYS[(days + 4).rem_euclid(7) as usize],
        MONTHS[(month - 1) as usize],
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

pub fn now_iso() -> String {
    iso(SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn iso_times_are_utc_with_milliseconds() {
        assert_eq!(iso(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let t = UNIX_EPOCH + Duration::from_millis(1_793_437_200_123);
        assert_eq!(iso(t), "2026-10-31T09:00:00.123Z");
        assert_eq!(iso(UNIX_EPOCH + Duration::from_millis(1_793_352_900_123)), "2026-10-30T09:35:00.123Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(iso(leap), "2000-02-29T00:00:00.000Z");
        // Before 1970 is 1970.
        assert_eq!(iso(UNIX_EPOCH - Duration::from_secs(5)), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn ctime_is_utc_with_the_weekday() {
        assert_eq!(ctime(0), "Thu Jan  1 00:00:00 1970");
        assert_eq!(ctime(1_793_437_200), "Sat Oct 31 09:00:00 2026");
        assert_eq!(ctime(951_782_400), "Tue Feb 29 00:00:00 2000");
        assert_eq!(ctime(-5), "Thu Jan  1 00:00:00 1970");
    }
}
