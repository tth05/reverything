//! Size and date filters in queries: `size:>1gb`, `size:1mb..5mb`, `size:empty`, `dm:today`,
//! `dc:2024`, `dm:>=2024-05-01`, `dm:2024-01..2024-03`. `dm:` is the modification date, `dc:`
//! the creation date, both in local time.
//!
//! A comparison with a period works on its edges: `dm:>2024` is after the end of 2024,
//! `dm:>=2024` from its start, `dm:<2024` before its start and `dm:<=2024` until its end.

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Time::{SystemTimeToFileTime, TzSpecificLocalTimeToSystemTime};

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
                let today = local_today();
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

/// A calendar day in local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Day {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

impl Day {
    fn new(year: i32, month: u32, day: u32) -> Self {
        Self { year, month, day }
    }

    /// Days since 1970-01-01 (proleptic Gregorian calendar).
    fn number(self) -> i64 {
        let y = if self.month <= 2 {
            self.year - 1
        } else {
            self.year
        } as i64;
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let m = self.month as i64;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + self.day as i64 - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146097 + doe - 719468
    }

    fn from_number(n: i64) -> Self {
        let z = n + 719468;
        let era = z.div_euclid(146097);
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let year = (yoe + era * 400 + if month <= 2 { 1 } else { 0 }) as i32;
        Self::new(year, month, day)
    }

    fn add_days(self, days: i64) -> Self {
        Self::from_number(self.number() + days)
    }

    /// 0 for Monday
    fn weekday(self) -> i64 {
        (self.number() + 3).rem_euclid(7)
    }

    fn next_month(self) -> Self {
        if self.month == 12 {
            Self::new(self.year + 1, 1, 1)
        } else {
            Self::new(self.year, self.month + 1, 1)
        }
    }
}

/// A period of days, `start` included and `end` excluded.
fn parse_period(s: &str, today: Day) -> Option<(Day, Day)> {
    let s = s.trim();
    let month_start = Day::new(today.year, today.month, 1);
    let week_start = today.add_days(-today.weekday());
    let period = match s {
        "today" => (today, today.add_days(1)),
        "yesterday" => (today.add_days(-1), today),
        "thisweek" => (week_start, week_start.add_days(7)),
        "lastweek" => (week_start.add_days(-7), week_start),
        "thismonth" => (month_start, month_start.next_month()),
        "lastmonth" => {
            let last = month_start.add_days(-1);
            (Day::new(last.year, last.month, 1), month_start)
        }
        "thisyear" => (Day::new(today.year, 1, 1), Day::new(today.year + 1, 1, 1)),
        "lastyear" => (Day::new(today.year - 1, 1, 1), Day::new(today.year, 1, 1)),
        _ => {
            let parts = s.split('-').collect::<Vec<_>>();
            let number = |i: usize| parts.get(i)?.parse::<u32>().ok();
            match parts.len() {
                1 => {
                    let year = number(0)? as i32;
                    (Day::new(year, 1, 1), Day::new(year + 1, 1, 1))
                }
                2 => {
                    let start = Day::new(number(0)? as i32, number(1)?, 1);
                    (start, start.next_month())
                }
                3 => {
                    let start = Day::new(number(0)? as i32, number(1)?, number(2)?);
                    (start, start.add_days(1))
                }
                _ => return None,
            }
        }
    };
    let valid = |d: Day| (1..=12).contains(&d.month) && (1..=31).contains(&d.day);
    (valid(period.0) && valid(period.1) && (1601..=9999).contains(&period.0.year)).then_some(period)
}

/// The inclusive range of unix timestamps of a period of local days.
fn to_unix_range((start, end): (Day, Day)) -> (u64, u64) {
    let start = local_midnight_unix(start).max(0) as u64;
    let end = local_midnight_unix(end).max(1) as u64;
    (start, end - 1)
}

fn local_today() -> Day {
    let now = unsafe { GetLocalTime() };
    Day::new(now.wYear as i32, now.wMonth as u32, now.wDay as u32)
}

/// Unix time of the local midnight starting `day`, following the time zone's rules for that
/// date (daylight saving time).
fn local_midnight_unix(day: Day) -> i64 {
    let local = SYSTEMTIME {
        wYear: day.year as u16,
        wMonth: day.month as u16,
        wDay: day.day as u16,
        ..Default::default()
    };
    let mut utc = SYSTEMTIME::default();
    let fallback = day.number() * 86400;
    unsafe {
        if TzSpecificLocalTimeToSystemTime(None, &local, &mut utc).is_err() {
            return fallback;
        }
        let mut ft = FILETIME::default();
        if SystemTimeToFileTime(&utc, &mut ft).is_err() {
            return fallback;
        }
        let ticks = ((ft.dwHighDateTime as i64) << 32) | ft.dwLowDateTime as i64;
        // FILETIME counts 100 ns since 1601
        ticks / 10_000_000 - 11_644_473_600
    }
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
    fn days() {
        for (y, m, d) in [(1970, 1, 1), (2000, 2, 29), (2024, 12, 31), (1601, 1, 1)] {
            let day = Day::new(y, m, d);
            assert_eq!(Day::from_number(day.number()), day);
        }
        assert_eq!(Day::new(1970, 1, 2).number(), 1);
        // 2026-10-05 is a Monday
        assert_eq!(Day::new(2026, 10, 5).weekday(), 0);
    }

    #[test]
    fn periods() {
        let today = Day::new(2026, 10, 7); // Wednesday
        let p = |s: &str| parse_period(s, today);
        assert_eq!(p("today"), Some((today, Day::new(2026, 10, 8))));
        assert_eq!(
            p("thisweek"),
            Some((Day::new(2026, 10, 5), Day::new(2026, 10, 12)))
        );
        assert_eq!(
            p("lastmonth"),
            Some((Day::new(2026, 9, 1), Day::new(2026, 10, 1)))
        );
        assert_eq!(
            p("2024-12"),
            Some((Day::new(2024, 12, 1), Day::new(2025, 1, 1)))
        );
        assert_eq!(
            p("2024-02-29"),
            Some((Day::new(2024, 2, 29), Day::new(2024, 3, 1)))
        );
        assert_eq!(p("2024-13"), None);
        assert_eq!(p("someday"), None);
    }
}
