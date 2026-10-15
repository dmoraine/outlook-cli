//! Conversions between FILETIME and user-facing timestamps.

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc};

use crate::error::{Error, Result};
use crate::store::{filetime_to_unix_nanos, unix_nanos_to_filetime};

pub fn now_ft() -> i64 {
    let n = Utc::now();
    unix_nanos_to_filetime(n.timestamp() as i128 * 1_000_000_000 + n.timestamp_subsec_nanos() as i128)
}

fn to_datetime(ft: i64) -> Option<DateTime<Utc>> {
    let ns = filetime_to_unix_nanos(ft);
    let secs = ns.div_euclid(1_000_000_000) as i64;
    let sub = ns.rem_euclid(1_000_000_000) as u32;
    DateTime::from_timestamp(secs, sub)
}

/// RFC 3339 with full (100 ns) precision, so that a printed timestamp used as
/// a cursor round-trips exactly.
pub fn format_ft(ft: i64, local: bool) -> Option<String> {
    if ft <= 0 {
        return None;
    }
    let dt = to_datetime(ft)?;
    Some(if local {
        dt.with_timezone(&Local).to_rfc3339_opts(SecondsFormat::AutoSi, false)
    } else {
        dt.to_rfc3339_opts(SecondsFormat::AutoSi, true)
    })
}

fn from_datetime<Tz: TimeZone>(dt: DateTime<Tz>) -> i64 {
    unix_nanos_to_filetime(dt.timestamp() as i128 * 1_000_000_000 + dt.timestamp_subsec_nanos() as i128)
}

fn local(naive: NaiveDateTime) -> Result<i64> {
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(from_datetime)
        .ok_or_else(|| Error::Usage(format!("invalid local time {naive}")))
}

/// Parse a point in time: RFC 3339 (`2026-10-15T08:00:00Z`, `...+02:00`),
/// local date/time (`2026-10-15 08:00`, `2026-10-15`), a duration back from
/// now (`90s`, `15m`, `2h`, `3d`, `1w`), or `now` / `today` / `yesterday`.
pub fn parse_when(s: &str) -> Result<i64> {
    let s = s.trim();
    let lower = s.to_ascii_lowercase();
    let today = || Local::now().date_naive().and_hms_opt(0, 0, 0).unwrap();
    match lower.as_str() {
        "now" => return Ok(now_ft()),
        "today" => return local(today()),
        "yesterday" => return local(today() - chrono::Duration::days(1)),
        _ => {}
    }
    let split = lower.find(|c: char| !c.is_ascii_digit()).unwrap_or(lower.len());
    if split > 0 && split < lower.len() {
        let n: i64 = lower[..split].parse().map_err(|_| Error::Usage(format!("bad duration {s}")))?;
        let unit = lower[split..].trim();
        let secs = match unit {
            "s" | "sec" | "secs" | "second" | "seconds" => Some(1),
            "m" | "min" | "mins" | "minute" | "minutes" => Some(60),
            "h" | "hour" | "hours" => Some(3600),
            "d" | "day" | "days" => Some(86_400),
            "w" | "week" | "weeks" => Some(604_800),
            _ => None,
        };
        if let Some(secs) = secs {
            return Ok(now_ft() - n * secs * 10_000_000);
        }
    }
    let iso = s.replacen(' ', "T", 1);
    if let Ok(dt) = DateTime::parse_from_rfc3339(&iso) {
        return Ok(from_datetime(dt));
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(&iso, f) {
            return local(n);
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return local(d.and_hms_opt(0, 0, 0).unwrap());
    }
    Err(Error::Usage(format!(
        "cannot parse time '{s}' (use e.g. 2026-10-15T08:00:00Z, '2026-10-15 08:00', 2026-10-15, 2h, 3d, today)"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_full_precision() {
        let ft = 134363450128590763;
        let s = format_ft(ft, false).unwrap();
        assert_eq!(s, "2026-10-13T06:03:32.859076300Z");
        assert_eq!(parse_when(&s).unwrap(), ft);
        assert_eq!(format_ft(134363449880000000, false).unwrap(), "2026-10-13T06:03:08Z");
    }

    #[test]
    fn relative_and_offsets() {
        let a = parse_when("2026-10-13T08:03:08+02:00").unwrap();
        assert_eq!(a, 134363449880000000);
        let h = parse_when("2h").unwrap();
        assert!((now_ft() - h - 2 * 3600 * 10_000_000).abs() < 10_000_000);
        assert!(parse_when("2026-10-13").is_ok());
        assert!(parse_when("2026-10-13 08:00").is_ok());
        assert!(parse_when("garbage").is_err());
    }
}
