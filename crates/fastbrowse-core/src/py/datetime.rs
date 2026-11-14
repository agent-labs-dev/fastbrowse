//! `datetime` as pydantic reads and writes it, without a date library.
//!
//! The written form is the contract: `2026-01-02T03:04:05Z` for UTC, `+05:30` for another offset, nothing for a
//! naive time, and six fractional digits whenever there are microseconds. Reading takes what pydantic takes in JSON:
//! RFC 3339 and its `T`-less and seconds-less forms, a bare date, and a Unix timestamp in seconds or milliseconds.

use std::fmt;

use serde::de::Error;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DateTime {
    pub year: i32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub microsecond: u32,
    /// Seconds east of UTC; `None` for a naive time.
    pub offset: Option<i32>,
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const WEEKDAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i32, month: u8, day: u8) -> i64 {
    let y = i64::from(year) - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i32, u8, u8) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
    (year, month, day)
}

fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 31,
    }
}

impl DateTime {
    pub fn new(year: i32, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Self {
        DateTime { year, month, day, hour, minute, second, microsecond: 0, offset: None }
    }

    pub fn utc(self) -> Self {
        DateTime { offset: Some(0), ..self }
    }

    /// The current time in UTC, as `datetime.now(UTC)`.
    pub fn now_utc() -> Self {
        let elapsed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        Self::from_timestamp(elapsed.as_secs() as i64, elapsed.subsec_micros())
    }

    /// A Unix timestamp as an aware UTC time.
    pub fn from_timestamp(seconds: i64, microsecond: u32) -> Self {
        let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
        let of_day = seconds.rem_euclid(86_400);
        DateTime {
            year,
            month,
            day,
            hour: (of_day / 3600) as u8,
            minute: (of_day / 60 % 60) as u8,
            second: (of_day % 60) as u8,
            microsecond,
            offset: Some(0),
        }
    }

    /// Monday is 0, as `date.weekday()`.
    pub fn weekday(&self) -> usize {
        (days_from_civil(self.year, self.month, self.day) + 3).rem_euclid(7) as usize
    }

    pub fn weekday_name(&self) -> &'static str {
        WEEKDAYS[self.weekday()]
    }

    pub fn month_name(&self) -> &'static str {
        MONTHS[usize::from(self.month) - 1]
    }

    /// This date moved by whole days, the time of day kept.
    pub fn add_days(&self, days: i64) -> Self {
        let (year, month, day) = civil_from_days(days_from_civil(self.year, self.month, self.day) + days);
        DateTime { year, month, day, ..*self }
    }

    /// `replace(day=1)`.
    pub fn first_of_month(&self) -> Self {
        DateTime { day: 1, ..*self }
    }

    /// `%Y-%m-%d`.
    pub fn date_string(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// pydantic's JSON form.
    pub fn isoformat(&self) -> String {
        let mut out = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        );
        if self.microsecond != 0 {
            out.push_str(&format!(".{:06}", self.microsecond));
        }
        match self.offset {
            None => {}
            Some(0) => out.push('Z'),
            Some(offset) => {
                let sign = if offset < 0 { '-' } else { '+' };
                let magnitude = offset.unsigned_abs();
                out.push_str(&format!("{sign}{:02}:{:02}", magnitude / 3600, magnitude / 60 % 60));
                if magnitude % 60 != 0 {
                    out.push_str(&format!(":{:02}", magnitude % 60));
                }
            }
        }
        out
    }

    /// What pydantic reads as a datetime in JSON.
    pub fn parse(value: &Value) -> Result<Self, String> {
        match value {
            Value::Number(n) => n.as_f64().map(Self::from_unix).ok_or_else(invalid),
            Value::String(s) => Self::parse_str(s),
            _ => Err(invalid()),
        }
    }

    // pydantic reads a timestamp above 2e10 as milliseconds.
    fn from_unix(value: f64) -> Self {
        let seconds = if value.abs() > 2e10 { value / 1000.0 } else { value };
        let whole = seconds.floor();
        let micros = ((seconds - whole) * 1e6).round() as u32;
        Self::from_timestamp(whole as i64, micros.min(999_999))
    }

    pub fn parse_str(text: &str) -> Result<Self, String> {
        // A string of digits is a timestamp to pydantic, as the number would be.
        let unsigned = text.strip_prefix('-').unwrap_or(text);
        if !unsigned.is_empty()
            && unsigned.bytes().all(|b| b.is_ascii_digit() || b == b'.')
            && let Ok(number) = text.parse::<f64>()
        {
            return Ok(Self::from_unix(number));
        }
        let bytes = text.as_bytes();
        let digits = |range: std::ops::Range<usize>| -> Option<u32> {
            let part = text.get(range)?;
            (part.bytes().all(|b| b.is_ascii_digit())).then(|| part.parse().ok())?
        };
        let year = digits(0..4).ok_or_else(invalid)? as i32;
        if bytes.get(4) != Some(&b'-') || bytes.get(7) != Some(&b'-') {
            return Err(invalid());
        }
        let month = digits(5..7).ok_or_else(invalid)? as u8;
        let day = digits(8..10).ok_or_else(invalid)? as u8;
        if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
            return Err(invalid());
        }
        let mut result = DateTime::new(year, month, day, 0, 0, 0);
        if bytes.len() == 10 {
            return Ok(result);
        }
        if !matches!(bytes[10], b'T' | b't' | b' ' | b'_') {
            return Err(invalid());
        }
        result.hour = digits(11..13).ok_or_else(invalid)? as u8;
        if bytes.get(13) != Some(&b':') {
            return Err(invalid());
        }
        result.minute = digits(14..16).ok_or_else(invalid)? as u8;
        let mut at = 16;
        if bytes.get(at) == Some(&b':') {
            result.second = digits(at + 1..at + 3).ok_or_else(invalid)? as u8;
            at += 3;
            if matches!(bytes.get(at), Some(b'.' | b',')) {
                let start = at + 1;
                let mut end = start;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                if end == start {
                    return Err(invalid());
                }
                // Digits past the sixth are dropped, not rounded.
                let fraction = &text[start..end.min(start + 6)];
                result.microsecond =
                    fraction.parse::<u32>().map_err(|_| invalid())? * 10u32.pow(6 - fraction.len() as u32);
                at = end;
            }
        }
        if result.hour > 23 || result.minute > 59 || result.second > 59 {
            return Err(invalid());
        }
        let rest = &text[at..];
        result.offset = match rest {
            "" => None,
            "Z" | "z" => Some(0),
            _ => {
                let sign = match rest.as_bytes()[0] {
                    b'+' => 1,
                    b'-' => -1,
                    _ => return Err(invalid()),
                };
                let body = rest[1..].replace(':', "");
                if body.len() != 4 || !body.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(invalid());
                }
                let hours: i32 = body[..2].parse().map_err(|_| invalid())?;
                let minutes: i32 = body[2..].parse().map_err(|_| invalid())?;
                if hours > 23 || minutes > 59 {
                    return Err(invalid());
                }
                Some(sign * (hours * 3600 + minutes * 60))
            }
        };
        Ok(result)
    }
}

fn invalid() -> String {
    "Input should be a valid datetime".into()
}

impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.isoformat())
    }
}

impl Serialize for DateTime {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.isoformat())
    }
}

impl<'de> Deserialize<'de> for DateTime {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        DateTime::parse(&Value::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn datetimes_read_and_write_as_pydantic_does() {
        for (input, written) in [
            (json!("2026-01-02T03:04:05"), "2026-01-02T03:04:05"),
            (json!("2026-01-02 03:04:05"), "2026-01-02T03:04:05"),
            (json!("2026-01-02T03:04"), "2026-01-02T03:04:00"),
            (json!("2026-01-02T03:04:05.1234567Z"), "2026-01-02T03:04:05.123456Z"),
            (json!("2026-01-02T03:04:05+0530"), "2026-01-02T03:04:05+05:30"),
            (json!("2026-01-02T03:04:05-00:00"), "2026-01-02T03:04:05Z"),
            (json!(1700000000), "2023-11-14T22:13:20Z"),
            (json!(1700000000.5), "2023-11-14T22:13:20.500000Z"),
            (json!(1700000000000_i64), "2023-11-14T22:13:20Z"),
            (json!("1700000000"), "2023-11-14T22:13:20Z"),
            (json!("2026-01-02"), "2026-01-02T00:00:00"),
            (json!("2026-01-02T03:04:05.123+05:30"), "2026-01-02T03:04:05.123000+05:30"),
            (json!("2026-01-02t03:04:05z"), "2026-01-02T03:04:05Z"),
            (json!(0), "1970-01-01T00:00:00Z"),
            (json!(-1), "1969-12-31T23:59:59Z"),
        ] {
            assert_eq!(DateTime::parse(&input).map(|d| d.isoformat()).as_deref(), Ok(written), "{input}");
        }
        assert!(DateTime::parse(&json!("2026-1-2")).is_err());
        assert!(DateTime::parse(&json!("2026-02-30")).is_err());
    }

    #[test]
    fn a_date_knows_its_weekday() {
        assert_eq!(DateTime::new(2026, 9, 27, 0, 0, 0).weekday_name(), "Sunday");
        assert_eq!(DateTime::new(2026, 9, 27, 0, 0, 0).add_days(32).date_string(), "2026-10-29");
    }
}
