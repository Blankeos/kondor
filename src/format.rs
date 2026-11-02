//! Small formatting helpers for sizes, ages, counts and paths.

use std::path::Path;

/// Decimal units, matching what Finder and most disk tools report.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 999.95 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if value < 9.95 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;
const WEEK: u64 = 7 * DAY;
const MONTH: u64 = 30 * DAY;
const YEAR: u64 = 365 * DAY;

/// Compact age for table cells: `now`, `5m`, `3h`, `2d`, `3w`, `4mo`, `2y`.
pub fn age_short(secs: u64) -> String {
    match secs {
        s if s < MINUTE => "now".into(),
        s if s < HOUR => format!("{}m", s / MINUTE),
        s if s < DAY => format!("{}h", s / HOUR),
        s if s < 2 * WEEK => format!("{}d", s / DAY),
        s if s < 2 * MONTH => format!("{}w", s / WEEK),
        s if s < YEAR => format!("{}mo", s / MONTH),
        s => format!("{}y", s / YEAR),
    }
}

/// Long age for prose: `just now`, `1 minute ago`, `3 weeks ago`.
pub fn age_long(secs: u64) -> String {
    let (n, unit) = match secs {
        s if s < MINUTE => return "just now".into(),
        s if s < HOUR => (s / MINUTE, "minute"),
        s if s < DAY => (s / HOUR, "hour"),
        s if s < 2 * WEEK => (s / DAY, "day"),
        s if s < 2 * MONTH => (s / WEEK, "week"),
        s if s < YEAR => (s / MONTH, "month"),
        s => (s / YEAR, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// Parse an age like `30d`, `6w`, `90m`, `2M`, `1y` into seconds.
pub fn parse_age(input: &str) -> Result<u64, String> {
    let input = input.trim();
    let unit_at = input
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("add a unit, e.g. {input}d (m, h, d, w, M or y)"))?;
    let (digits, unit) = input.split_at(unit_at);
    let n: u64 = digits.parse().map_err(|_| "expected a number then a unit, e.g. 30d".to_string())?;
    let mult = match unit {
        "m" => MINUTE,
        "h" => HOUR,
        "d" => DAY,
        "w" => WEEK,
        "M" => MONTH,
        "y" => YEAR,
        _ => return Err(format!("unknown unit '{unit}': use m, h, d, w, M or y (e.g. 30d)")),
    };
    Ok(n.saturating_mul(mult))
}

/// `12403` -> `12,403`.
pub fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{} {}", count(n as u64), if n == 1 { one } else { many })
}

/// Display a path with the home folder shortened to `~`.
pub fn tilde(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home {
        if let Ok(rest) = path.strip_prefix(home) {
            return if rest.as_os_str().is_empty() {
                "~".into()
            } else {
                format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display())
            };
        }
    }
    path.display().to_string()
}

/// Keep the end of a string, prefixing `…` if it had to be cut.
pub fn truncate_left(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let tail: String = s.chars().skip(len - (width - 1)).collect();
    format!("…{tail}")
}

/// Keep the start of a string, suffixing `…` if it had to be cut.
pub fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let head: String = s.chars().take(width - 1).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(999), "999 B");
        assert_eq!(size(1_000), "1.0 KB");
        assert_eq!(size(4_200_000_000), "4.2 GB");
        assert_eq!(size(312_400_000), "312 MB");
        assert_eq!(size(999_990), "1.0 MB");
    }

    #[test]
    fn ages() {
        assert_eq!(age_short(0), "now");
        assert_eq!(age_short(2 * DAY), "2d");
        assert_eq!(age_short(3 * WEEK), "3w");
        assert_eq!(age_short(4 * MONTH), "4mo");
        assert_eq!(age_long(MINUTE), "1 minute ago");
        assert_eq!(age_long(3 * YEAR), "3 years ago");
    }

    #[test]
    fn parses_ages() {
        assert_eq!(parse_age("30d"), Ok(30 * DAY));
        assert_eq!(parse_age("2M"), Ok(2 * MONTH));
        assert_eq!(parse_age("0m"), Ok(0));
        assert!(parse_age("30").is_err());
        assert!(parse_age("d").is_err());
        assert!(parse_age("3x").is_err());
    }

    #[test]
    fn counts_and_truncation() {
        assert_eq!(count(12_403), "12,403");
        assert_eq!(count(999), "999");
        assert_eq!(truncate_left("abcdef", 4), "…def");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
    }
}
