//! When a trigger is next due: durations, cron expressions and timezones.

use std::str::FromStr;

use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;
use croner::parser::{CronParser, Seconds, Year};
use croner::Cron;

/// Parse `90s`, `30m`, `2h`, `1d`, `1w`, `500ms`, or a compound such as
/// `1h30m`, into milliseconds. A bare number is seconds.
pub fn parse_duration(text: &str) -> Result<u64, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("a duration is required, for example \"30m\"".to_string());
    }
    if let Ok(seconds) = text.parse::<u64>() {
        return Ok(seconds.saturating_mul(1000));
    }
    let mut total: u64 = 0;
    let mut digits = String::new();
    let mut unit = String::new();
    let mut seen = false;
    let flush = |digits: &mut String, unit: &mut String, total: &mut u64| -> Result<(), String> {
        let value: f64 = digits
            .parse()
            .map_err(|_| format!("\"{text}\" is not a duration (try \"30m\" or \"1h30m\")"))?;
        let scale = match unit.to_ascii_lowercase().as_str() {
            "ms" => 1.0,
            "s" | "sec" | "secs" | "second" | "seconds" => 1000.0,
            "m" | "min" | "mins" | "minute" | "minutes" => 60_000.0,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000.0,
            "d" | "day" | "days" => 86_400_000.0,
            "w" | "week" | "weeks" => 604_800_000.0,
            other => {
                return Err(format!(
                    "unknown duration unit \"{other}\" in \"{text}\" (use ms, s, m, h, d or w)"
                ))
            }
        };
        *total = total.saturating_add((value * scale).round() as u64);
        digits.clear();
        unit.clear();
        Ok(())
    };
    for ch in text.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            if !unit.is_empty() {
                flush(&mut digits, &mut unit, &mut total)?;
                seen = true;
            }
            digits.push(ch);
        } else if ch.is_ascii_alphabetic() {
            if digits.is_empty() {
                return Err(format!(
                    "\"{text}\" is not a duration (try \"30m\" or \"1h30m\")"
                ));
            }
            unit.push(ch);
        } else if !ch.is_whitespace() {
            return Err(format!(
                "\"{text}\" is not a duration (try \"30m\" or \"1h30m\")"
            ));
        }
    }
    if !digits.is_empty() {
        if unit.is_empty() {
            return Err(format!(
                "\"{text}\" is missing a unit (use ms, s, m, h, d or w)"
            ));
        }
        flush(&mut digits, &mut unit, &mut total)?;
        seen = true;
    }
    if !seen {
        return Err(format!(
            "\"{text}\" is not a duration (try \"30m\" or \"1h30m\")"
        ));
    }
    Ok(total)
}

/// Render milliseconds the way [`parse_duration`] reads them.
pub fn format_duration(ms: u64) -> String {
    const UNITS: [(u64, &str); 5] = [
        (86_400_000, "d"),
        (3_600_000, "h"),
        (60_000, "m"),
        (1000, "s"),
        (1, "ms"),
    ];
    if ms == 0 {
        return "0s".to_string();
    }
    let mut left = ms;
    let mut out = String::new();
    for (size, unit) in UNITS {
        if left >= size {
            out.push_str(&format!("{}{unit}", left / size));
            left %= size;
        }
    }
    out
}

/// A parsed 5-field cron expression (names and the `@hourly`-style macros
/// included) in a timezone.
#[derive(Debug, Clone)]
pub struct CronSchedule {
    cron: Cron,
    zone: Tz,
}

impl CronSchedule {
    pub fn parse(expression: &str, timezone: Option<&str>) -> Result<CronSchedule, String> {
        let zone = match timezone.map(str::trim).filter(|zone| !zone.is_empty()) {
            None => Tz::UTC,
            Some(name) => Tz::from_str(name).map_err(|_| {
                format!("unknown timezone \"{name}\" (use an IANA name such as \"Europe/Paris\")")
            })?,
        };
        let expression = expression.trim();
        if expression.eq_ignore_ascii_case("@reboot") {
            return Err("@reboot is not supported".to_string());
        }
        let cron = CronParser::builder()
            .seconds(Seconds::Disallowed)
            .year(Year::Disallowed)
            .build()
            .parse(expression)
            .map_err(|error| format!("invalid cron expression \"{expression}\": {error}"))?;
        Ok(CronSchedule { cron, zone })
    }

    /// The first occurrence strictly after `after_ms`.
    pub fn next_after(&self, after_ms: u64) -> Option<u64> {
        let after = self.local(after_ms)?;
        let next = self.cron.find_next_occurrence(&after, false).ok()?;
        u64::try_from(next.with_timezone(&Utc).timestamp_millis()).ok()
    }

    /// The last occurrence at or before `at_ms`.
    pub fn previous_at_or_before(&self, at_ms: u64) -> Option<u64> {
        let at = self.local(at_ms)?;
        let previous = self.cron.find_previous_occurrence(&at, true).ok()?;
        u64::try_from(previous.with_timezone(&Utc).timestamp_millis()).ok()
    }

    fn local(&self, ms: u64) -> Option<DateTime<Tz>> {
        let utc = Utc.timestamp_millis_opt(i64::try_from(ms).ok()?).single()?;
        Some(utc.with_timezone(&self.zone))
    }
}

/// Parse an RFC 3339 instant into unix milliseconds.
pub fn parse_instant(text: &str) -> Result<u64, String> {
    let parsed = DateTime::parse_from_rfc3339(text.trim()).map_err(|_| {
        format!("\"{text}\" is not an RFC 3339 time (for example 2027-01-31T09:00:00+01:00)")
    })?;
    u64::try_from(parsed.timestamp_millis())
        .map_err(|_| format!("\"{text}\" is before 1970 and cannot be scheduled"))
}

/// Render unix milliseconds as RFC 3339 in UTC.
pub fn format_instant(ms: u64) -> String {
    i64::try_from(ms)
        .ok()
        .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| ms.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> u64 {
        parse_instant(text).unwrap()
    }

    #[test]
    fn durations_parse_and_render() {
        assert_eq!(parse_duration("30m").unwrap(), 1_800_000);
        assert_eq!(parse_duration("60s").unwrap(), 60_000);
        assert_eq!(parse_duration("1h30m").unwrap(), 5_400_000);
        assert_eq!(parse_duration("2 hours").unwrap(), 7_200_000);
        assert_eq!(parse_duration("250ms").unwrap(), 250);
        assert_eq!(parse_duration("1.5h").unwrap(), 5_400_000);
        assert_eq!(parse_duration("45").unwrap(), 45_000);
        assert_eq!(parse_duration("1d").unwrap(), 86_400_000);
        for bad in ["", "m", "10x", "ten minutes", "5m3", "-5m"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
        assert_eq!(format_duration(5_400_000), "1h30m");
        assert_eq!(format_duration(250), "250ms");
        assert_eq!(
            parse_duration(&format_duration(93_784_000)).unwrap(),
            93_784_000
        );
    }

    #[test]
    fn cron_accepts_five_fields_names_and_macros() {
        let weekdays = CronSchedule::parse("0 9 * * MON-FRI", None).unwrap();
        // 2027-01-01 is a Friday.
        assert_eq!(
            weekdays.next_after(at("2027-01-01T09:00:00Z")),
            Some(at("2027-01-04T09:00:00Z"))
        );
        let numeric = CronSchedule::parse("0 9 * * 1-5", None).unwrap();
        assert_eq!(
            numeric.next_after(at("2027-01-01T09:00:00Z")),
            Some(at("2027-01-04T09:00:00Z"))
        );
        let hourly = CronSchedule::parse("@hourly", None).unwrap();
        assert_eq!(
            hourly.next_after(at("2027-01-01T09:20:00Z")),
            Some(at("2027-01-01T10:00:00Z"))
        );
        let daily = CronSchedule::parse("@daily", Some("Asia/Tokyo")).unwrap();
        assert_eq!(
            daily.next_after(at("2027-01-01T00:00:00Z")),
            Some(at("2027-01-01T15:00:00Z"))
        );
        let weekly = CronSchedule::parse("@weekly", None).unwrap();
        assert_eq!(
            weekly.next_after(at("2027-01-01T00:00:00Z")),
            Some(at("2027-01-03T00:00:00Z"))
        );
        let months = CronSchedule::parse("30 6 1 JAN,JUL *", None).unwrap();
        assert_eq!(
            months.next_after(at("2027-01-02T00:00:00Z")),
            Some(at("2027-07-01T06:30:00Z"))
        );
        assert_eq!(
            hourly.previous_at_or_before(at("2027-01-01T09:20:00Z")),
            Some(at("2027-01-01T09:00:00Z"))
        );
    }

    #[test]
    fn cron_rejects_what_it_cannot_schedule() {
        for bad in [
            "",
            "* * * *",
            "0 0 9 * * *",
            "61 * * * *",
            "@reboot",
            "nope",
        ] {
            assert!(CronSchedule::parse(bad, None).is_err(), "{bad}");
        }
        let error = CronSchedule::parse("0 9 * * *", Some("Mars/Olympus")).unwrap_err();
        assert!(error.contains("unknown timezone"), "{error}");
    }

    #[test]
    fn a_daily_cron_keeps_its_local_hour_across_dst_changes() {
        let paris = CronSchedule::parse("0 9 * * *", Some("Europe/Paris")).unwrap();
        // Spring forward: 2027-03-28, 02:00 CET becomes 03:00 CEST.
        assert_eq!(
            paris.next_after(at("2027-03-27T08:00:00Z")),
            Some(at("2027-03-28T07:00:00Z"))
        );
        assert_eq!(
            paris.next_after(at("2027-03-26T08:00:00Z")),
            Some(at("2027-03-27T08:00:00Z"))
        );
        // Fall back: 2027-10-31, 03:00 CEST becomes 02:00 CET.
        assert_eq!(
            paris.next_after(at("2027-10-30T07:00:00Z")),
            Some(at("2027-10-31T08:00:00Z"))
        );
    }

    #[test]
    fn a_cron_inside_the_dst_gap_or_overlap_fires_once_a_day() {
        let paris = CronSchedule::parse("30 2 * * *", Some("Europe/Paris")).unwrap();
        // 02:30 does not exist on 2027-03-28; the run is not lost.
        let mut now = at("2027-03-27T02:00:00Z");
        let mut fired = Vec::new();
        for _ in 0..3 {
            now = paris.next_after(now).unwrap();
            fired.push(format_instant(now));
        }
        assert_eq!(fired.len(), 3);
        assert_eq!(fired[0], "2027-03-28T01:00:00Z", "{fired:?}");
        assert_eq!(fired[1], "2027-03-29T00:30:00Z", "{fired:?}");
        // 02:30 happens twice on 2027-10-31; the run happens once.
        let mut now = at("2027-10-30T12:00:00Z");
        let mut fired = Vec::new();
        for _ in 0..2 {
            now = paris.next_after(now).unwrap();
            fired.push(format_instant(now));
        }
        assert_eq!(fired, ["2027-10-31T00:30:00Z", "2027-11-01T01:30:00Z"]);
    }

    #[test]
    fn instants_round_trip() {
        let ms = at("2027-01-31T09:00:00+01:00");
        assert_eq!(format_instant(ms), "2027-01-31T08:00:00Z");
        assert!(parse_instant("tomorrow").is_err());
    }
}
