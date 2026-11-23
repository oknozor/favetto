//! A small, dependency-light cron parser and next-occurrence calculator.
//!
//! This replaces `croner` and `tokio-cron-scheduler`, both of which are
//! hard-wired to the unmaintained `chrono` crate. The parser intentionally
//! keeps the configuration the daemon relied on before:
//!
//! - **seconds are required** — expressions have six fields
//!   (`sec min hour day-of-month month day-of-week`), with an optional seventh
//!   year field;
//! - when both day-of-month and day-of-week are restricted, **both must match**
//!   (`dom_and_dow(true)`);
//! - all arithmetic is in UTC.
//!
//! Supported syntax per field: `*`, `?`, lists (`a,b`), ranges (`a-b`), steps
//! (`*/n`, `a-b/n`) and three-letter month/weekday names (`JAN`, `MON`).

use favetto_wire::Timestamp;
use time::{Date, Duration, Month, PrimitiveDateTime, Weekday};

/// How far ahead [`Cron::next_after`] searches for an occurrence.
///
/// Five years covers every calendar pattern expressible without a year field,
/// including a February 29th that only occurs every four years.
const MAX_SEARCH_DAYS: u32 = 366 * 5;

/// A parse error for a cron expression.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CronError {
    /// The expression did not have six (or seven) whitespace-separated fields.
    #[error(
        "cron expression must have 6 fields (seconds, minutes, hours, day-of-month, month, day-of-week), got {0}"
    )]
    FieldCount(usize),
    /// A field could not be parsed or was outside its valid range.
    #[error("invalid {field} field '{value}': {reason}")]
    Field {
        /// The cron field that failed.
        field: &'static str,
        /// The raw field text.
        value: String,
        /// Why it failed.
        reason: String,
    },
}

/// A parsed cron expression, evaluated in UTC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    seconds: Vec<u8>,
    minutes: Vec<u8>,
    hours: Vec<u8>,
    days_of_month: Vec<u8>,
    /// 1-based month numbers.
    months: Vec<u8>,
    /// 0-based weekday numbers, with Sunday = 0 (input `7` is normalized).
    days_of_week: Vec<u8>,
    /// Years for an optional seventh field, or `None` for every year.
    years: Option<Vec<i32>>,
    dom_restricted: bool,
    dow_restricted: bool,
}

impl Cron {
    /// Parse a six- or seven-field cron expression.
    pub fn parse(expression: &str) -> Result<Self, CronError> {
        let expression = expand_macro(expression.trim());
        let fields: Vec<&str> = expression.split_whitespace().collect();
        if fields.len() != 6 && fields.len() != 7 {
            return Err(CronError::FieldCount(fields.len()));
        }

        let (seconds, _) = parse_numeric_field(fields[0], "seconds", 0, 59, &[])?;
        let (minutes, _) = parse_numeric_field(fields[1], "minutes", 0, 59, &[])?;
        let (hours, _) = parse_numeric_field(fields[2], "hours", 0, 23, &[])?;
        let (days_of_month, dom_restricted) =
            parse_numeric_field(fields[3], "day-of-month", 1, 31, &[])?;
        let (months, _) = parse_numeric_field(fields[4], "month", 1, 12, MONTH_NAMES)?;
        let (days_of_week, dow_restricted) =
            parse_numeric_field(fields[5], "day-of-week", 0, 7, WEEKDAY_NAMES)?;
        // Normalize `7` (Sunday) to `0` so both spellings match.
        let mut days_of_week: Vec<u8> = days_of_week
            .into_iter()
            .map(|day| if day == 7 { 0 } else { day as u8 })
            .collect();
        days_of_week.sort_unstable();
        days_of_week.dedup();

        let years = if fields.len() == 7 {
            let (years, _) = parse_numeric_field(fields[6], "year", 1970, 9999, &[])?;
            Some(years.into_iter().map(i32::from).collect())
        } else {
            None
        };

        let seconds = to_u8(seconds);
        let minutes = to_u8(minutes);
        let hours = to_u8(hours);
        let days_of_month = to_u8(days_of_month);
        let months = to_u8(months);

        Ok(Self {
            seconds,
            minutes,
            hours,
            days_of_month,
            months,
            days_of_week,
            years,
            dom_restricted,
            dow_restricted,
        })
    }

    /// The next occurrence strictly after `after`, or `None` when the
    /// expression cannot fire again inside the search window.
    pub fn next_after(&self, after: Timestamp) -> Option<Timestamp> {
        let after_dt = after.to_offset_datetime();
        let start = after_dt.date();
        let after_second_of_day = u32::from(after_dt.hour()) * 3600
            + u32::from(after_dt.minute()) * 60
            + u32::from(after_dt.second());
        let search_days = self.search_days(start);

        for offset in 0..search_days {
            let date = start.checked_add(Duration::days(i64::from(offset)))?;
            if !self.year_matches(date) || !self.month_matches(date) || !self.day_matches(date) {
                continue;
            }
            for hour in &self.hours {
                for minute in &self.minutes {
                    for second in &self.seconds {
                        let second_of_day =
                            u32::from(*hour) * 3600 + u32::from(*minute) * 60 + u32::from(*second);
                        // On the first day, the candidate must be strictly later
                        // than `after`; this also skips the sub-second remainder.
                        if offset == 0 && second_of_day <= after_second_of_day {
                            continue;
                        }
                        let time = time::Time::from_hms(*hour, *minute, *second).ok()?;
                        let candidate = PrimitiveDateTime::new(date, time).assume_utc();
                        return Some(Timestamp::new(candidate));
                    }
                }
            }
        }
        None
    }

    fn year_matches(&self, date: Date) -> bool {
        match &self.years {
            Some(years) => years.contains(&date.year()),
            None => true,
        }
    }

    /// How many days to scan: five years normally, or exactly up to the last
    /// year in a seven-field expression.
    fn search_days(&self, start: Date) -> u32 {
        let Some(years) = &self.years else {
            return MAX_SEARCH_DAYS;
        };
        let Some(last) = years.iter().copied().max() else {
            return 0;
        };
        let Ok(end) = Date::from_calendar_date(last, Month::December, 31) else {
            return MAX_SEARCH_DAYS;
        };
        let days = (end - start).whole_days();
        u32::try_from(days.max(0) + 1).unwrap_or(MAX_SEARCH_DAYS)
    }

    fn month_matches(&self, date: Date) -> bool {
        self.months.contains(&u8::from(date.month()))
    }

    fn day_matches(&self, date: Date) -> bool {
        let dom_ok = self.days_of_month.contains(&date.day());
        let dow_ok = self.days_of_week.contains(&weekday_number(date.weekday()));
        match (self.dom_restricted, self.dow_restricted) {
            // Both restricted: require both, matching the daemon's previous
            // `dom_and_dow(true)` configuration.
            (true, true) => dom_ok && dow_ok,
            (true, false) => dom_ok,
            (false, true) => dow_ok,
            (false, false) => true,
        }
    }
}

/// The next UTC occurrence of `expression` strictly after `now`, or `None` when
/// the expression does not parse or has no future occurrence.
pub fn next_run_after(expression: &str, now: Timestamp) -> Option<Timestamp> {
    Cron::parse(expression).ok()?.next_after(now)
}

/// Whether `expression` is a valid cron expression.
pub fn is_valid(expression: &str) -> bool {
    Cron::parse(expression).is_ok()
}

fn weekday_number(weekday: Weekday) -> u8 {
    weekday.number_days_from_sunday()
}

fn expand_macro(expression: &str) -> &str {
    match expression.to_ascii_lowercase().as_str() {
        "@yearly" | "@annually" => "0 0 0 1 1 *",
        "@monthly" => "0 0 0 1 * *",
        "@weekly" => "0 0 0 * * 0",
        "@daily" | "@midnight" => "0 0 0 * * *",
        "@hourly" => "0 0 * * * *",
        _ => expression,
    }
}

const MONTH_NAMES: &[(&str, u16)] = &[
    ("jan", 1),
    ("feb", 2),
    ("mar", 3),
    ("apr", 4),
    ("may", 5),
    ("jun", 6),
    ("jul", 7),
    ("aug", 8),
    ("sep", 9),
    ("oct", 10),
    ("nov", 11),
    ("dec", 12),
];

const WEEKDAY_NAMES: &[(&str, u16)] = &[
    ("sun", 0),
    ("mon", 1),
    ("tue", 2),
    ("wed", 3),
    ("thu", 4),
    ("fri", 5),
    ("sat", 6),
];

/// Parse one cron field into a sorted, de-duplicated list of values.
///
/// The second return value reports whether the field was "restricted" (not a
/// bare wildcard), which drives the day-of-month/day-of-week combination rule.
fn parse_numeric_field(
    spec: &str,
    field: &'static str,
    min: u16,
    max: u16,
    names: &[(&str, u16)],
) -> Result<(Vec<u16>, bool), CronError> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(field_error(field, spec, "field is empty"));
    }
    if spec == "*" || spec == "?" {
        return Ok(((min..=max).collect(), false));
    }

    let mut values = Vec::new();
    for part in spec.split(',') {
        if part.is_empty() {
            return Err(field_error(field, spec, "empty list entry"));
        }
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u16 = step
                    .parse()
                    .map_err(|_| field_error(field, spec, "step must be a number"))?;
                if step == 0 {
                    return Err(field_error(field, spec, "step must be greater than zero"));
                }
                (range, Some(step))
            }
            None => (part, None),
        };

        let (start, end) = if range == "*" || range == "?" {
            (min, max)
        } else if let Some((start, end)) = range.split_once('-') {
            (
                parse_value(start, field, spec, min, max, names)?,
                parse_value(end, field, spec, min, max, names)?,
            )
        } else {
            let value = parse_value(range, field, spec, min, max, names)?;
            // `a/n` means "from a to the field maximum, every n".
            match step {
                Some(_) => (value, max),
                None => (value, value),
            }
        };

        if start > end {
            return Err(field_error(field, spec, "range start is after its end"));
        }
        let step = step.unwrap_or(1);
        let mut value = start;
        while value <= end {
            values.push(value);
            value += step;
        }
    }

    values.sort_unstable();
    values.dedup();
    Ok((values, true))
}

fn parse_value(
    raw: &str,
    field: &'static str,
    spec: &str,
    min: u16,
    max: u16,
    names: &[(&str, u16)],
) -> Result<u16, CronError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(field_error(field, spec, "missing value"));
    }
    let value = if let Ok(value) = raw.parse::<u16>() {
        value
    } else {
        let lower = raw.to_ascii_lowercase();
        names
            .iter()
            .find(|(name, _)| *name == lower)
            .map(|(_, value)| *value)
            .ok_or_else(|| field_error(field, spec, "unknown name"))?
    };
    if value < min || value > max {
        return Err(field_error(
            field,
            spec,
            &format!("value {value} is outside {min}-{max}"),
        ));
    }
    Ok(value)
}

fn to_u8(values: Vec<u16>) -> Vec<u8> {
    values.into_iter().map(|value| value as u8).collect()
}

fn field_error(field: &'static str, value: &str, reason: &str) -> CronError {
    CronError::Field {
        field,
        value: value.to_string(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(value: &str) -> Timestamp {
        value.parse().unwrap()
    }

    #[test]
    fn handles_second_and_minute_forms() {
        let now = utc("2026-01-05T00:00:00Z");
        assert_eq!(
            next_run_after("0 * * * * *", now),
            Some(utc("2026-01-05T00:01:00Z"))
        );
        assert_eq!(
            next_run_after("30 0 * * * *", now),
            Some(utc("2026-01-05T00:00:30Z"))
        );
        assert_eq!(
            next_run_after("*/15 * * * * *", now),
            Some(utc("2026-01-05T00:00:15Z"))
        );
    }

    #[test]
    fn handles_daily_and_weekly_forms() {
        let now = utc("2026-01-05T00:00:00Z");
        assert_eq!(
            next_run_after("0 0 8 * * *", now),
            Some(utc("2026-01-05T08:00:00Z"))
        );
        // 2026-01-05 is a Monday.
        assert_eq!(
            next_run_after("0 30 9 * * 1", now),
            Some(utc("2026-01-05T09:30:00Z"))
        );
        assert_eq!(
            next_run_after("0 30 9 * * MON", now),
            Some(utc("2026-01-05T09:30:00Z"))
        );
    }

    #[test]
    fn rejects_unparseable_expressions() {
        let now = utc("2026-01-05T00:00:00Z");
        assert_eq!(next_run_after("not a cron", now), None);
        // Five-field expressions fail the seconds-required parser.
        assert_eq!(next_run_after("0 8 * * *", now), None);
        assert_eq!(next_run_after("60 * * * * *", now), None);
        assert_eq!(next_run_after("* * * 0 * *", now), None);
        assert_eq!(next_run_after("*/0 * * * * *", now), None);
        assert_eq!(next_run_after("5-1 * * * * *", now), None);
        assert!(!is_valid("nope"));
        assert!(is_valid("@daily"));
    }

    #[test]
    fn requires_both_restricted_day_fields_to_match() {
        // The 13th, when it is also a Friday. 2026-02-13 is a Friday.
        let now = utc("2026-01-01T00:00:00Z");
        assert_eq!(
            next_run_after("0 0 0 13 * 5", now),
            Some(utc("2026-02-13T00:00:00Z"))
        );
        // When only one field is restricted the other is a wildcard.
        assert_eq!(
            next_run_after("0 0 0 13 * *", now),
            Some(utc("2026-01-13T00:00:00Z"))
        );
    }

    #[test]
    fn handles_month_names_and_ranges() {
        assert_eq!(
            next_run_after("0 0 0 1 JAN *", utc("2026-01-01T00:00:00Z")),
            Some(utc("2027-01-01T00:00:00Z"))
        );
        assert_eq!(
            next_run_after("0 0 12 * * MON-FRI", utc("2026-01-03T00:00:00Z")),
            Some(utc("2026-01-05T12:00:00Z"))
        );
    }

    #[test]
    fn handles_sunday_spelled_as_seven() {
        // 2026-01-04 is a Sunday.
        assert_eq!(
            next_run_after("0 0 0 * * 7", utc("2026-01-01T00:00:00Z")),
            Some(utc("2026-01-04T00:00:00Z"))
        );
        assert_eq!(
            next_run_after("0 0 0 * * 0", utc("2026-01-01T00:00:00Z")),
            Some(utc("2026-01-04T00:00:00Z"))
        );
    }

    #[test]
    fn finds_leap_day() {
        assert_eq!(
            next_run_after("0 0 0 29 2 *", utc("2026-01-01T00:00:00Z")),
            Some(utc("2028-02-29T00:00:00Z"))
        );
    }

    #[test]
    fn is_strictly_after_now() {
        let now = utc("2026-01-05T00:00:00Z");
        assert_eq!(
            next_run_after("0 0 0 * * *", now),
            Some(utc("2026-01-06T00:00:00Z"))
        );
    }

    #[test]
    fn supports_an_optional_year_field() {
        assert_eq!(
            next_run_after("0 0 0 1 1 * 2030", utc("2026-01-01T00:00:00Z")),
            Some(utc("2030-01-01T00:00:00Z"))
        );
        assert_eq!(
            next_run_after("0 0 0 1 1 * 2020", utc("2026-01-01T00:00:00Z")),
            None
        );
        // A year inside a far-future expression is still found.
        assert_eq!(
            next_run_after("0 0 0 1 1 * 2040", utc("2026-01-01T00:00:00Z")),
            Some(utc("2040-01-01T00:00:00Z"))
        );
    }
}
