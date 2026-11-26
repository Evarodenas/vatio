//! Time handling. Everything in the Spanish peninsular market runs on Europe/Madrid
//! wall-clock days, which are 23 or 25 hours long twice a year, so day boundaries are
//! always computed through the time zone database rather than by adding 24 hours.

use std::sync::LazyLock;

use jiff::{SignedDuration, Timestamp, civil::Date, tz::TimeZone};
use schemars::JsonSchema;
use serde::Serialize;

use crate::error::{Error, Result};

pub const ZONE: &str = "Europe/Madrid";

static MADRID: LazyLock<TimeZone> = LazyLock::new(|| {
    // The time zone database is compiled into the binary (`tzdb-bundle-always`), so
    // this cannot fail at run time, even in containers with no /usr/share/zoneinfo.
    TimeZone::get(ZONE).expect("bundled tzdb contains Europe/Madrid")
});

pub fn madrid() -> TimeZone {
    MADRID.clone()
}

/// An instant, serialised as RFC 3339 with the Europe/Madrid offset in force at that
/// instant (for example `2026-10-25T02:00:00+01:00`). The offset makes the two 02:00
/// hours of the autumn clock change distinguishable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalTime(pub Timestamp);

impl LocalTime {
    pub fn rfc3339(self) -> String {
        self.0
            .to_zoned_datetime(madrid())
            .strftime("%Y-%m-%dT%H:%M:%S%:z")
            .to_string()
    }

    /// Wall-clock `HH:MM` in Madrid.
    pub fn hhmm(self) -> String {
        self.0
            .to_zoned_datetime(madrid())
            .strftime("%H:%M")
            .to_string()
    }

    pub fn date(self) -> Date {
        self.0.to_zoned_datetime(madrid()).date()
    }
}

impl Serialize for LocalTime {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.rfc3339())
    }
}

impl JsonSchema for LocalTime {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        "LocalTime".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "format": "date-time",
            "description": "RFC 3339 timestamp carrying the Europe/Madrid UTC offset"
        })
    }
}

/// `[start, end)` of a Madrid calendar day as instants.
pub fn day_bounds(date: Date) -> Result<(Timestamp, Timestamp)> {
    let tz = madrid();
    let out_of_range = |_| Error::usage(format!("the date {date} is out of range"));
    let start = date.to_zoned_datetime(tz.clone()).map_err(out_of_range)?;
    let end = date
        .tomorrow()
        .and_then(|d| d.to_zoned_datetime(tz))
        .map_err(out_of_range)?;
    Ok((start.timestamp(), end.timestamp()))
}

pub fn today(now: Timestamp) -> Date {
    LocalTime(now).date()
}

/// Parse a day argument: `today`, `tomorrow`, `yesterday` or `YYYY-MM-DD` (Madrid calendar).
pub fn parse_day(input: &str, now: Timestamp) -> Result<Date> {
    let today = today(now);
    let day = match input.trim().to_lowercase().as_str() {
        "today" | "hoy" => Ok(today),
        "tomorrow" | "mañana" | "manana" => today.tomorrow(),
        "yesterday" | "ayer" => today.yesterday(),
        other => other.parse::<Date>(),
    };
    day.map_err(|_| {
        Error::usage(format!(
            "invalid day '{input}': expected today, tomorrow, yesterday or YYYY-MM-DD"
        ))
    })
}

/// Parse a run length such as `3h`, `90m`, `2h30m`, `1h 15m` or ISO 8601 `PT45M`.
/// A bare number means hours. Returns whole seconds.
pub fn parse_duration(input: &str) -> Result<i64> {
    let text = input.trim();
    let bad = || {
        Error::usage(format!(
            "invalid duration '{input}': use 3h, 90m, 2h30m or PT45M; a bare number means hours. \
             Whole minutes only, from 1 minute to 24 hours"
        ))
    };
    let secs = if let Ok(hours) = text.parse::<f64>() {
        hours * 3600.0
    } else {
        text.parse::<SignedDuration>()
            .map_err(|_| bad())?
            .as_secs_f64()
    };
    let secs = secs.round();
    if !(60.0..=86_400.0).contains(&secs) || secs % 60.0 != 0.0 {
        return Err(bad());
    }
    Ok(secs as i64)
}

/// The start of the minute containing `at`.
pub fn floor_minute(at: Timestamp) -> Timestamp {
    Timestamp::from_second(at.as_second() - at.as_second().rem_euclid(60)).unwrap_or(at)
}

/// Parse a point in time for `--from` / `--until`:
/// `HH:MM` (that Madrid wall-clock time today if it is the current minute or later,
/// otherwise tomorrow; `24:00` is the coming midnight),
/// `YYYY-MM-DDTHH:MM` (Madrid wall clock), or a full RFC 3339 timestamp.
pub fn parse_when(input: &str, now: Timestamp) -> Result<Timestamp> {
    let text = input.trim();
    let bad = || {
        Error::usage(format!(
            "invalid time '{input}': use HH:MM, YYYY-MM-DDTHH:MM or an RFC 3339 timestamp"
        ))
    };
    if let Ok(ts) = text.parse::<Timestamp>() {
        return Ok(ts);
    }
    let tz = madrid();
    if let Ok(dt) = text.parse::<jiff::civil::DateTime>() {
        return Ok(dt.to_zoned_datetime(tz).map_err(|_| bad())?.timestamp());
    }
    let today = today(now);
    if text == "24:00" {
        return day_bounds(today).map(|(_, end)| end);
    }
    let time = jiff::civil::Time::strptime("%H:%M", text).map_err(|_| bad())?;
    let candidate = today
        .to_datetime(time)
        .to_zoned_datetime(tz.clone())
        .map_err(|_| bad())?;
    if candidate.timestamp() >= floor_minute(now) {
        return Ok(candidate.timestamp());
    }
    let next = today.tomorrow().map_err(|_| bad())?;
    Ok(next
        .to_datetime(time)
        .to_zoned_datetime(tz)
        .map_err(|_| bad())?
        .timestamp())
}

/// Human-readable run length: `3h`, `90m` stays `1h30m`, `45m`.
pub fn fmt_duration(secs: i64) -> String {
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    match (h, m) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h{m:02}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn local_time_carries_the_madrid_offset_in_winter_and_summer() {
        assert_eq!(
            LocalTime(ts("2026-11-26T18:00:00Z")).rfc3339(),
            "2026-11-26T19:00:00+01:00"
        );
        assert_eq!(
            LocalTime(ts("2026-07-01T18:00:00Z")).rfc3339(),
            "2026-07-01T20:00:00+02:00"
        );
    }

    #[test]
    fn autumn_clock_change_day_is_25_hours_and_repeats_02h() {
        let (start, end) = day_bounds("2026-10-25".parse().unwrap()).unwrap();
        assert_eq!((end - start).as_hours(), 25);
        // 00:00Z and 01:00Z are both 02:00 on the wall clock; the offset tells them apart.
        assert_eq!(
            LocalTime(ts("2026-10-25T00:00:00Z")).rfc3339(),
            "2026-10-25T02:00:00+02:00"
        );
        assert_eq!(
            LocalTime(ts("2026-10-25T01:00:00Z")).rfc3339(),
            "2026-10-25T02:00:00+01:00"
        );
    }

    #[test]
    fn spring_clock_change_day_is_23_hours() {
        let (start, end) = day_bounds("2026-03-29".parse().unwrap()).unwrap();
        assert_eq!((end - start).as_hours(), 23);
    }

    #[test]
    fn day_words_use_the_madrid_calendar_not_utc() {
        // 23:30 UTC on the 25th is already the 26th in Madrid.
        let now = ts("2026-11-25T23:30:00Z");
        assert_eq!(parse_day("today", now).unwrap().to_string(), "2026-11-26");
        assert_eq!(
            parse_day("tomorrow", now).unwrap().to_string(),
            "2026-11-27"
        );
        assert_eq!(parse_day("ayer", now).unwrap().to_string(), "2026-11-25");
        assert_eq!(
            parse_day("2026-01-02", now).unwrap().to_string(),
            "2026-01-02"
        );
        assert!(parse_day("26/11/2026", now).is_err());
    }

    #[test]
    fn durations_accept_common_forms() {
        assert_eq!(parse_duration("3h").unwrap(), 10800);
        assert_eq!(parse_duration("90m").unwrap(), 5400);
        assert_eq!(parse_duration("2h30m").unwrap(), 9000);
        assert_eq!(parse_duration("PT45M").unwrap(), 2700);
        assert_eq!(parse_duration("2").unwrap(), 7200);
        assert_eq!(parse_duration("1.5").unwrap(), 5400);
        assert_eq!(parse_duration("0.1").unwrap(), 360);
        // Whole minutes only: seconds would not survive into `duration_minutes`.
        for bad in [
            "", "0", "30s", "25h", "-2h", "soon", "PT90S", "1.01", "90", "nan",
        ] {
            assert!(parse_duration(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn bare_clock_times_mean_the_next_occurrence() {
        let now = ts("2026-11-26T18:40:00Z"); // 19:40 in Madrid
        assert_eq!(
            LocalTime(parse_when("22:00", now).unwrap()).rfc3339(),
            "2026-11-26T22:00:00+01:00"
        );
        assert_eq!(
            LocalTime(parse_when("08:00", now).unwrap()).rfc3339(),
            "2026-11-27T08:00:00+01:00"
        );
        assert_eq!(
            LocalTime(parse_when("2026-11-28T07:30", now).unwrap()).rfc3339(),
            "2026-11-28T07:30:00+01:00"
        );
        assert_eq!(
            parse_when("2026-11-27T06:00:00Z", now).unwrap(),
            ts("2026-11-27T06:00:00Z")
        );
        assert!(parse_when("teatime", now).is_err());
        // The current minute still counts as today, and 24:00 is the coming midnight.
        assert_eq!(
            LocalTime(parse_when("19:40", ts("2026-11-26T18:40:27Z")).unwrap()).rfc3339(),
            "2026-11-26T19:40:00+01:00"
        );
        assert_eq!(
            LocalTime(parse_when("24:00", now).unwrap()).rfc3339(),
            "2026-11-27T00:00:00+01:00"
        );
    }

    #[test]
    fn day_words_ignore_case_and_far_dates_are_usage_errors() {
        let now = ts("2026-11-26T10:00:00Z");
        assert_eq!(parse_day("MAÑANA", now).unwrap().to_string(), "2026-11-27");
        assert_eq!(parse_day("Today", now).unwrap().to_string(), "2026-11-26");
        let far = day_bounds("9999-12-31".parse().unwrap()).unwrap_err();
        assert_eq!(far.exit_code(), 2);
    }

    #[test]
    fn durations_format_compactly() {
        assert_eq!(fmt_duration(10800), "3h");
        assert_eq!(fmt_duration(5400), "1h30m");
        assert_eq!(fmt_duration(2700), "45m");
    }
}
