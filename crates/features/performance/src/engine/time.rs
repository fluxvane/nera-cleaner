//! Human-readable times for uptime and "last run" labels.

use chrono::{Datelike as _, Local, TimeZone as _};

/// Current Unix time in seconds.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Uptime as "3d 4h", "5h 12m", "12m 3s".
pub fn uptime_label(secs: u64) -> String {
    let days = secs / 86_400;
    if days > 0 {
        format!("{days}d {}h", (secs % 86_400) / 3_600)
    } else {
        nera_core::format::duration_secs(secs)
    }
}

/// "Just now", "5 min ago", "3 hours ago", "1 day ago", "4 days ago",
/// otherwise the date ("12 Mar 2026").
pub fn relative_label(then: i64, now: i64) -> String {
    let delta = now - then;
    if delta < 60 {
        // Includes small clock skew into the future.
        return "Just now".into();
    }
    let mins = delta / 60;
    if mins < 60 {
        return format!("{mins} min ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return if hours == 1 {
            "1 hour ago".into()
        } else {
            format!("{hours} hours ago")
        };
    }
    // Elapsed 24-hour periods, not calendar days: 30 hours ago can be two
    // calendar days back, so "Yesterday" would be wrong.
    let days = hours / 24;
    if days < 30 {
        return if days == 1 {
            "1 day ago".into()
        } else {
            format!("{days} days ago")
        };
    }
    date_label(then)
}

/// Local date, e.g. "12 Mar 2026".
pub fn date_label(unix: i64) -> String {
    match Local.timestamp_opt(unix, 0).single() {
        Some(t) => t.format("%-d %b %Y").to_string(),
        None => "Unknown".into(),
    }
}

/// Local date and time, e.g. "9 Nov, 08:58"; the year is added when it is not
/// the current one ("9 Nov 2025, 08:58").
pub fn date_time_label(unix: i64) -> String {
    date_time_label_in(unix, Local::now().year())
}

fn date_time_label_in(unix: i64, current_year: i32) -> String {
    match Local.timestamp_opt(unix, 0).single() {
        Some(t) if t.year() == current_year => t.format("%-d %b, %H:%M").to_string(),
        Some(t) => t.format("%-d %b %Y, %H:%M").to_string(),
        None => "Unknown".into(),
    }
}

/// Value shown for a maintenance task: when it last succeeded.
pub fn last_run_label(last_run: Option<i64>, now: i64) -> String {
    match last_run {
        Some(at) => relative_label(at, now),
        None => "Never run".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_labels() {
        assert_eq!(uptime_label(42), "42s");
        assert_eq!(uptime_label(3_725), "1h 2m");
        assert_eq!(uptime_label(3 * 86_400 + 4 * 3_600 + 59), "3d 4h");
    }

    #[test]
    fn relative_labels() {
        let now = 1_800_000_000;
        assert_eq!(relative_label(now, now), "Just now");
        assert_eq!(relative_label(now + 30, now), "Just now");
        assert_eq!(relative_label(now - 5 * 60, now), "5 min ago");
        assert_eq!(relative_label(now - 3_600, now), "1 hour ago");
        assert_eq!(relative_label(now - 5 * 3_600, now), "5 hours ago");
        assert_eq!(
            relative_label(now - 23 * 3_600 - 59 * 60, now),
            "23 hours ago"
        );
        assert_eq!(relative_label(now - 24 * 3_600, now), "1 day ago");
        assert_eq!(relative_label(now - 30 * 3_600, now), "1 day ago");
        assert_eq!(relative_label(now - 48 * 3_600, now), "2 days ago");
        assert_eq!(relative_label(now - 4 * 86_400, now), "4 days ago");
        let old = relative_label(now - 90 * 86_400, now);
        assert!(old.chars().next().unwrap().is_ascii_digit(), "{old}");
        assert!(old.ends_with("2026"), "{old}");
    }

    #[test]
    fn date_labels_are_formatted() {
        assert!(date_time_label(1_800_000_000).contains(':'));
        assert!(!date_label(0).is_empty());
    }

    #[test]
    fn date_time_label_adds_year_only_when_needed() {
        let t = Local
            .with_ymd_and_hms(2026, 11, 9, 8, 58, 0)
            .unwrap()
            .timestamp();
        assert_eq!(date_time_label_in(t, 2026), "9 Nov, 08:58");
        assert_eq!(date_time_label_in(t, 2027), "9 Nov 2026, 08:58");
    }

    #[test]
    fn last_run_labels() {
        assert_eq!(last_run_label(None, 1_800_000_000), "Never run");
        assert_eq!(
            last_run_label(Some(1_800_000_000 - 120), 1_800_000_000),
            "2 min ago"
        );
    }
}
