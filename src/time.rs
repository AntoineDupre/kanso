//! UTC formatting without a date-time dependency.

/// Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

fn split(millis: i64) -> (i64, u32, u32, i64, i64, i64, i64) {
    let seconds = millis.div_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let rem = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    (
        year,
        month,
        day,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        millis.rem_euclid(1000),
    )
}

/// `YYYY-MM-DD HH:MM:SSZ` in UTC.
pub fn format_utc_millis(millis: i64) -> String {
    let (year, month, day, hour, minute, second, _) = split(millis);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}Z")
}

/// `YYYYMMDD-HHMMSS` in UTC, for generated file names.
pub fn file_stamp(millis: i64) -> String {
    let (year, month, day, hour, minute, second, _) = split(millis);
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_the_unix_epoch_and_known_dates() {
        assert_eq!(format_utc_millis(0), "1970-01-01 00:00:00Z");
        assert_eq!(file_stamp(0), "19700101-000000");
        // 2026-11-03T12:34:56Z
        assert_eq!(format_utc_millis(1_793_709_296_000), "2026-11-03 12:34:56Z");
    }

    #[test]
    fn handles_leap_days_and_milliseconds() {
        // 2024-02-29T23:59:59.999Z
        assert_eq!(format_utc_millis(1_709_251_199_999), "2024-02-29 23:59:59Z");
        assert_eq!(file_stamp(1_709_251_199_999), "20240229-235959");
    }

    #[test]
    fn handles_pre_epoch_values() {
        assert_eq!(format_utc_millis(-1_000), "1969-12-31 23:59:59Z");
    }
}
