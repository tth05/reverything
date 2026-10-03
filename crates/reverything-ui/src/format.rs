//! Display formatting of sizes, dates, attributes and durations.

use std::time::Duration;

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

pub fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{} B", bytes);
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{:.1} {}", value, UNITS[unit])
    } else {
        format!("{:.0} {}", value, UNITS[unit])
    }
}

/// Formats a unix timestamp in local time.
pub fn time(unix: u32) -> String {
    if unix == 0 {
        return String::new();
    }
    let ticks = (unix as u64 + 11_644_473_600) * 10_000_000;
    let ft = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    unsafe {
        if FileTimeToSystemTime(&ft, &mut utc).is_err()
            || SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_err()
        {
            return String::new();
        }
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute
    )
}

/// Explorer style attribute letters.
pub fn attributes(flags: u32) -> String {
    const LETTERS: [(u32, char); 9] = [
        (0x1, 'R'),    // read only
        (0x2, 'H'),    // hidden
        (0x4, 'S'),    // system
        (0x20, 'A'),   // archive
        (0x100, 'T'),  // temporary
        (0x200, 'P'),  // sparse
        (0x400, 'L'),  // reparse point
        (0x800, 'C'),  // compressed
        (0x4000, 'E'), // encrypted
    ];
    LETTERS
        .iter()
        .filter(|(bit, _)| flags & bit != 0)
        .map(|&(_, c)| c)
        .collect()
}

pub fn micros(us: u64) -> String {
    duration(Duration::from_micros(us))
}

pub fn duration(d: Duration) -> String {
    let us = d.as_micros();
    if us < 1_000 {
        format!("{} µs", us)
    } else if us < 1_000_000 {
        format!("{:.1} ms", us as f64 / 1000.0)
    } else if us < 120_000_000 {
        format!("{:.2} s", us as f64 / 1e6)
    } else if us < 7_200_000_000 {
        format!("{} min", us / 60_000_000)
    } else {
        format!("{:.1} h", us as f64 / 3.6e9)
    }
}

/// "3 min ago" for a unix timestamp in seconds.
pub fn ago(unix: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let secs = now.saturating_sub(unix);
    match secs {
        0..=4 => "just now".into(),
        5..=119 => format!("{} s ago", secs),
        120..=7199 => format!("{} min ago", secs / 60),
        _ => format!("{} h ago", secs / 3600),
    }
}
