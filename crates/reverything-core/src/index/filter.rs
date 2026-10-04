//! Size and date filters in queries: `size:>1gb`, `size:1mb..5mb`, `size:empty`, `dm:today`,
//! `dc:2024`, `dm:>=2024-05-01`, `dm:2024-01..2024-03`. `dm:` is the modification date, `dc:`
//! the creation date, both in local time.
//!
//! A comparison with a period works on its edges: `dm:>2024` is after the end of 2024,
//! `dm:>=2024` from its start, `dm:<2024` before its start and `dm:<=2024` until its end.

use chrono::{Datelike, Days, Local, Months, NaiveDate, NaiveTime, TimeZone};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Size,
    Modified,
    Created,
}

/// Entries whose value of `field` is within `min..=max` match, or outside of it if `negate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Filter {
    pub field: Field,
    pub min: u64,
    pub max: u64,
    pub negate: bool,
}

impl Filter {
    #[inline]
    pub fn matches(&self, value: u64) -> bool {
        (self.min <= value && value <= self.max) != self.negate
    }

    /// Parses a filter token, `None` if it is not one (it is a normal search term then).
    pub fn parse(token: &str, negate: bool) -> Option<Self> {
        let (field, spec) = token.split_once(':')?;
        let field = match field.to_ascii_lowercase().as_str() {
            "size" => Field::Size,
            "dm" | "datemodified" => Field::Modified,
            "dc" | "datecreated" => Field::Created,
            _ => return None,
        };
        let spec = spec.trim().to_ascii_lowercase();
        let (min, max) = match field {
            Field::Size => parse_range(&spec, parse_size)?,
            Field::Modified | Field::Created => {
                let today = Local::now().date_naive();
                parse_range(&spec, |s| parse_period(s, today).map(to_unix_range))?
            }
        };
        Some(Filter {
            field,
            min,
            max,
            negate,
        })
    }
}

/// `a..b`, `<x`, `<=x`, `>x`, `>=x` or `x`, where every `x` is an inclusive range of values.
fn parse_range(spec: &str, value: impl Fn(&str) -> Option<(u64, u64)>) -> Option<(u64, u64)> {
    if let Some((a, b)) = spec.split_once("..") {
        return Some((value(a)?.0, value(b)?.1));
    }
    if let Some(v) = spec.strip_prefix(">=") {
        return Some((value(v)?.0, u64::MAX));
    }
    if let Some(v) = spec.strip_prefix("<=") {
        return Some((0, value(v)?.1));
    }
    if let Some(v) = spec.strip_prefix('>') {
        return Some((value(v)?.1.checked_add(1)?, u64::MAX));
    }
    if let Some(v) = spec.strip_prefix('<') {
        return Some((0, value(v)?.0.checked_sub(1)?));
    }
    value(spec.strip_prefix('=').unwrap_or(spec))
}

/// `1.5gb`, `10kb`, `300` (bytes), or `empty`.
fn parse_size(s: &str) -> Option<(u64, u64)> {
    let s = s.trim();
    if s == "empty" {
        return Some((0, 0));
    }
    let digits = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (number, unit) = s.split_at(digits);
    let number = number.parse::<f64>().ok()?;
    let unit = match unit.trim() {
        "" | "b" => 1u64,
        "k" | "kb" => 1 << 10,
        "m" | "mb" => 1 << 20,
        "g" | "gb" => 1 << 30,
        "t" | "tb" => 1 << 40,
        _ => return None,
    };
    let bytes = (number * unit as f64) as u64;
    Some((bytes, bytes))
}

/// A period of local days, `start` included and `end` excluded.
fn parse_period(s: &str, today: NaiveDate) -> Option<(NaiveDate, NaiveDate)> {
    let month_start = today.with_day(1)?;
    let week_start = today - Days::new(today.weekday().num_days_from_monday() as u64);
    let year_start = |year: i32| NaiveDate::from_ymd_opt(year, 1, 1);
    let next_month = |d: NaiveDate| d.checked_add_months(Months::new(1));
    Some(match s.trim() {
        "today" => (today, today.succ_opt()?),
        "yesterday" => (today.pred_opt()?, today),
        "thisweek" => (week_start, week_start + Days::new(7)),
        "lastweek" => (week_start - Days::new(7), week_start),
        "thismonth" => (month_start, next_month(month_start)?),
        "lastmonth" => (month_start.checked_sub_months(Months::new(1))?, month_start),
        "thisyear" => (year_start(today.year())?, year_start(today.year() + 1)?),
        "lastyear" => (year_start(today.year() - 1)?, year_start(today.year())?),
        s => {
            let parts = s
                .split('-')
                .map(|p| p.parse::<u32>().ok())
                .collect::<Option<Vec<_>>>()?;
            match parts[..] {
                [year] => (year_start(year as i32)?, year_start(year as i32 + 1)?),
                [year, month] => {
                    let start = NaiveDate::from_ymd_opt(year as i32, month, 1)?;
                    (start, next_month(start)?)
                }
                [year, month, day] => {
                    let start = NaiveDate::from_ymd_opt(year as i32, month, day)?;
                    (start, start.succ_opt()?)
                }
                _ => return None,
            }
        }
    })
}

/// The inclusive range of unix timestamps of a period of local days.
fn to_unix_range((start, end): (NaiveDate, NaiveDate)) -> (u64, u64) {
    let start = local_midnight_unix(start).max(0) as u64;
    let end = local_midnight_unix(end).max(1) as u64;
    (start, end - 1)
}

/// Unix time of the local midnight starting `day`, following the time zone's daylight saving
/// rules for that date.
fn local_midnight_unix(day: NaiveDate) -> i64 {
    let midnight = day.and_time(NaiveTime::MIN);
    // Where clocks jump at midnight, the day starts at the first valid time
    Local
        .from_local_datetime(&midnight)
        .earliest()
        .map_or_else(|| midnight.and_utc().timestamp(), |t| t.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        let f = |s: &str| Filter::parse(s, false).map(|f| (f.min, f.max));
        assert_eq!(f("size:>1kb"), Some((1025, u64::MAX)));
        assert_eq!(f("size:<=1.5mb"), Some((0, 1572864)));
        assert_eq!(f("size:1mb..2mb"), Some((1 << 20, 2 << 20)));
        assert_eq!(f("SIZE:empty"), Some((0, 0)));
        assert_eq!(f("size:lots"), None);
        assert_eq!(f("notafilter:1"), None);
        assert!(Filter::parse("size:>1gb", true).unwrap().matches(5));
    }

    #[test]
    fn periods() {
        let day = |y, m, d| NaiveDate::from_ymd_opt(y, m, d).unwrap();
        let today = day(2026, 10, 7); // Wednesday
        let p = |s: &str| parse_period(s, today);
        assert_eq!(p("today"), Some((today, day(2026, 10, 8))));
        assert_eq!(p("thisweek"), Some((day(2026, 10, 5), day(2026, 10, 12))));
        assert_eq!(p("lastmonth"), Some((day(2026, 9, 1), day(2026, 10, 1))));
        assert_eq!(p("2024-12"), Some((day(2024, 12, 1), day(2025, 1, 1))));
        assert_eq!(p("2024-02-29"), Some((day(2024, 2, 29), day(2024, 3, 1))));
        assert_eq!(p("2024-13"), None);
        assert_eq!(p("2023-02-29"), None);
        assert_eq!(p("someday"), None);
    }
}
