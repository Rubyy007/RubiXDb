//! `DATE`/`TIME`/`TIMESTAMP` literal text parsing — no new dependency
//! (`chrono`/`time` were deliberately not added; this project's own
//! established "zero new dependency beyond what's actually needed"
//! discipline applies here exactly as it does to the core engine crate).
//! `days_from_civil` is Howard Hinnant's well-known, correct-for-any-
//! proleptic-Gregorian-year algorithm — a small, self-contained,
//! independently testable piece of arithmetic, not a date library.

use crate::error::{Result, SqlError};

fn parse_error(detail: impl Into<String>) -> SqlError {
    SqlError::TypeMismatch { detail: detail.into() }
}

/// Days since the Unix epoch (1970-01-01) for a proleptic-Gregorian
/// civil date — Howard Hinnant's `days_from_civil` algorithm
/// (<https://howardhinnant.github.io/date_algorithms.html>), valid for
/// every `y`, including negative years and the algorithm's own
/// documented `i32`-range caveats (irrelevant here: `RelationalValue::
/// Date` is itself an `i32` day count, so any input producing a value
/// outside `i32` range is rejected by the caller's own `i32::try_from`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 }; // [0, 11]
    let doy = (153 * mp as i64 + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Parses `YYYY-MM-DD` into days-since-epoch, bounds-checked (never
/// panics on malformed/adversarial input, item 30's "controlled error,
/// never a panic" discipline applied to this new parsing surface too).
pub fn parse_date(text: &str) -> Result<i32> {
    let (y, m, d) = parse_date_parts(text)?;
    let days = days_from_civil(y, m, d);
    i32::try_from(days).map_err(|_| parse_error("DATE value is out of the representable range"))
}

fn parse_date_parts(text: &str) -> Result<(i64, u32, u32)> {
    let parts: Vec<&str> = text.split('-').collect();
    if parts.len() != 3 {
        return Err(parse_error("malformed DATE literal (expected YYYY-MM-DD)"));
    }
    let y: i64 = parts[0]
        .parse()
        .map_err(|_| parse_error("malformed DATE literal: year"))?;
    let m: u32 = parts[1]
        .parse()
        .map_err(|_| parse_error("malformed DATE literal: month"))?;
    let d: u32 = parts[2]
        .parse()
        .map_err(|_| parse_error("malformed DATE literal: day"))?;
    if !(1..=12).contains(&m) {
        return Err(parse_error("DATE month must be in 1..=12"));
    }
    let max_day = days_in_month(y, m);
    if d < 1 || d > max_day {
        return Err(parse_error("DATE day is out of range for its month"));
    }
    Ok((y, m, d))
}

/// Parses `HH:MM:SS[.ffffff]` into microseconds since midnight.
pub fn parse_time(text: &str) -> Result<i64> {
    let (h, min, s, micros) = parse_time_parts(text)?;
    Ok(((h as i64 * 3600 + min as i64 * 60 + s as i64) * 1_000_000) + micros as i64)
}

fn parse_time_parts(text: &str) -> Result<(u32, u32, u32, u32)> {
    let hms_and_frac: Vec<&str> = text.splitn(2, '.').collect();
    let hms: Vec<&str> = hms_and_frac[0].split(':').collect();
    if hms.len() != 3 {
        return Err(parse_error("malformed TIME literal (expected HH:MM:SS)"));
    }
    let h: u32 = hms[0].parse().map_err(|_| parse_error("malformed TIME literal: hour"))?;
    let m: u32 = hms[1]
        .parse()
        .map_err(|_| parse_error("malformed TIME literal: minute"))?;
    let s: u32 = hms[2]
        .parse()
        .map_err(|_| parse_error("malformed TIME literal: second"))?;
    if h > 23 || m > 59 || s > 59 {
        return Err(parse_error("TIME component out of range"));
    }
    let micros = if hms_and_frac.len() == 2 {
        let frac = hms_and_frac[1];
        if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) || frac.len() > 6 {
            return Err(parse_error("malformed TIME literal: fractional seconds"));
        }
        let padded = format!("{frac:0<6}");
        padded.parse().map_err(|_| parse_error("malformed TIME literal: fractional seconds"))?
    } else {
        0
    };
    Ok((h, m, s, micros))
}

/// Parses `YYYY-MM-DD[ T]HH:MM:SS[.ffffff]` into microseconds since the
/// Unix epoch.
pub fn parse_timestamp(text: &str) -> Result<i64> {
    let sep_pos = text
        .find([' ', 'T'])
        .ok_or_else(|| parse_error("malformed TIMESTAMP literal (expected 'DATE TIME')"))?;
    let (date_part, time_part) = text.split_at(sep_pos);
    let time_part = &time_part[1..];
    let (y, m, d) = parse_date_parts(date_part)?;
    let (h, min, s, micros) = parse_time_parts(time_part)?;
    let days = days_from_civil(y, m, d);
    let time_micros = ((h as i64 * 3600 + min as i64 * 60 + s as i64) * 1_000_000) + micros as i64;
    days.checked_mul(86_400_000_000)
        .and_then(|d| d.checked_add(time_micros))
        .ok_or_else(|| parse_error("TIMESTAMP value is out of the representable range"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_date_is_day_zero() {
        assert_eq!(parse_date("1970-01-01").unwrap(), 0);
    }

    #[test]
    fn known_dates_round_trip_to_expected_day_counts() {
        assert_eq!(parse_date("1969-12-31").unwrap(), -1);
        assert_eq!(parse_date("2000-03-01").unwrap(), 11017);
        assert_eq!(parse_date("2024-02-29").unwrap(), 19782); // 2024 is a leap year
    }

    #[test]
    fn rejects_invalid_calendar_dates_without_panicking() {
        assert!(parse_date("2023-02-29").is_err(), "2023 is not a leap year");
        assert!(parse_date("2024-13-01").is_err());
        assert!(parse_date("2024-00-01").is_err());
        assert!(parse_date("2024-01-32").is_err());
        assert!(parse_date("not-a-date").is_err());
        assert!(parse_date("").is_err());
        assert!(parse_date("2024-01").is_err());
    }

    #[test]
    fn time_round_trips_with_and_without_fraction() {
        assert_eq!(parse_time("00:00:00").unwrap(), 0);
        assert_eq!(parse_time("23:59:59").unwrap(), 86_399_000_000);
        assert_eq!(parse_time("12:00:00.5").unwrap(), 12 * 3_600_000_000 + 500_000);
        assert_eq!(parse_time("00:00:00.000001").unwrap(), 1);
    }

    #[test]
    fn rejects_invalid_times_without_panicking() {
        assert!(parse_time("24:00:00").is_err());
        assert!(parse_time("12:60:00").is_err());
        assert!(parse_time("12:00:60").is_err());
        assert!(parse_time("12:00").is_err());
        assert!(parse_time("garbage").is_err());
        assert!(parse_time("12:00:00.").is_err());
    }

    #[test]
    fn timestamp_combines_date_and_time() {
        assert_eq!(parse_timestamp("1970-01-01 00:00:00").unwrap(), 0);
        assert_eq!(parse_timestamp("1970-01-01T00:00:01").unwrap(), 1_000_000);
        assert_eq!(parse_timestamp("1969-12-31 23:59:59").unwrap(), -1_000_000);
    }

    #[test]
    fn rejects_malformed_timestamp_without_panicking() {
        assert!(parse_timestamp("1970-01-01").is_err());
        assert!(parse_timestamp("garbage").is_err());
        assert!(parse_timestamp("").is_err());
    }
}
