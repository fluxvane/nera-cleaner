//! Human-readable formatting shared by every screen.

/// Bytes as Finder shows them (decimal units: 1 KB = 1000 bytes), with three
/// significant digits ("1.50 MB", "54.1 GB", "995 GB"). The value is rounded
/// before its unit and digits are chosen, so a value just below a boundary
/// reads "1.00 MB", never "1000 KB", and "100 MB", never "100.0 MB".
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if n < 1000 {
        return if n == 1 {
            "1 byte".into()
        } else {
            format!("{n} bytes")
        };
    }
    let decimals = |v: f64, unit: usize| {
        if unit == 0 || v >= 100.0 {
            0
        } else if v >= 10.0 {
            1
        } else {
            2
        }
    };
    let (mut value, mut unit) = (n as f64 / 1000.0, 0usize);
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    loop {
        let factor = 10f64.powi(decimals(value, unit));
        let rounded = (value * factor).round() / factor;
        if rounded >= 1000.0 && unit < UNITS.len() - 1 {
            value = rounded / 1000.0;
            unit += 1;
            continue;
        }
        // Re-pick digits for the rounded value (9.996 -> "10.0", 99.96 -> "100").
        return format!(
            "{rounded:.*} {}",
            decimals(rounded, unit) as usize,
            UNITS[unit]
        );
    }
}

/// Memory as Activity Monitor and About This Mac show it: binary units
/// (1 GB = 1024³ bytes, so 64 GB of RAM reads "64.00 GB") with KB whole,
/// MB to one decimal and GB/TB to two. Rounded before the unit is chosen, so
/// it never prints "1024.0 MB". Use [`bytes`] for files and disks.
pub fn memory_bytes(n: u64) -> String {
    const UNITS: [(&str, i32); 4] = [("KB", 0), ("MB", 1), ("GB", 2), ("TB", 2)];
    if n < 1024 {
        return if n == 1 {
            "1 byte".into()
        } else {
            format!("{n} bytes")
        };
    }
    let (mut value, mut unit) = (n as f64 / 1024.0, 0usize);
    loop {
        let factor = 10f64.powi(UNITS[unit].1);
        let rounded = (value * factor).round() / factor;
        if rounded >= 1024.0 && unit + 1 < UNITS.len() {
            value /= 1024.0;
            unit += 1;
            continue;
        }
        return format!("{rounded:.*} {}", UNITS[unit].1 as usize, UNITS[unit].0);
    }
}

/// Signed byte delta, e.g. "+1.2 GB" / "-300 MB".
pub fn bytes_delta(n: i64) -> String {
    let sign = if n < 0 { "-" } else { "+" };
    format!("{sign}{}", bytes(n.unsigned_abs()))
}

/// Percentage 0–100 with no decimals.
pub fn percent(fraction: f64) -> String {
    format!("{:.0}%", (fraction.clamp(0.0, 1.0)) * 100.0)
}

/// Count with a unit noun, pluralised: `count(3, "item")` → "3 items".
pub fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Duration in seconds as "2h 5m" / "4m 10s" / "12s".
pub fn duration_secs(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes_like_finder() {
        assert_eq!(bytes(0), "0 bytes");
        assert_eq!(bytes(1), "1 byte");
        assert_eq!(bytes(999), "999 bytes");
        assert_eq!(bytes(1_500), "2 KB");
        assert_eq!(bytes(1_500_000), "1.50 MB");
        assert_eq!(bytes(54_070_000_000), "54.1 GB");
        assert_eq!(bytes(994_660_000_000), "995 GB");
    }

    #[test]
    fn rounds_before_choosing_the_unit() {
        assert_eq!(bytes(999_499), "999 KB");
        assert_eq!(bytes(999_500), "1.00 MB");
        assert_eq!(bytes(999_999_999), "1.00 GB");
        assert_eq!(bytes(99_960_000), "100 MB");
        assert_eq!(bytes(9_996_000), "10.0 MB");
        assert_eq!(bytes(99_940_000), "99.9 MB");
        assert_eq!(bytes(1_000_000_000_000_000_000), "1000 PB");
    }

    #[test]
    fn formats_memory_like_activity_monitor() {
        assert_eq!(memory_bytes(0), "0 bytes");
        assert_eq!(memory_bytes(1024), "1 KB");
        assert_eq!(memory_bytes(5 * 1024 * 1024 + 512 * 1024), "5.5 MB");
        assert_eq!(memory_bytes(68_719_476_736), "64.00 GB");
        assert_eq!(memory_bytes(1024 * 1024 - 1), "1.0 MB");
        assert_eq!(memory_bytes(1024 * 1024 * 1024 - 1), "1.00 GB");
        assert_eq!(memory_bytes(1024 * 1024 * 1024 - 60_000), "1023.9 MB");
        assert_eq!(memory_bytes((1 << 40) - 1), "1.00 TB");
    }

    #[test]
    fn formats_counts_and_durations() {
        assert_eq!(count(1, "item"), "1 item");
        assert_eq!(count(2, "item"), "2 items");
        assert_eq!(duration_secs(7500), "2h 5m");
        assert_eq!(duration_secs(250), "4m 10s");
        assert_eq!(percent(0.944), "94%");
    }
}
