//! Pure text shown by the Apps screen. Every number and label the screen
//! displays is produced here so it can be unit-tested and printed by
//! `examples/dump.rs` exactly as the UI would show it.

use crate::engine::{self, InstalledApp, LeftoverKind};
use nera_core::format;
use std::time::SystemTime;

/// Bytes in decimal units like Finder ("1.50 MB", "20.4 GB"), rounded first
/// and then given the digits of the rounded value, so a value just below a
/// unit boundary reads "1.00 MB", never "1000 KB" or "100.0 MB".
pub fn bytes(n: u64) -> String {
    format::bytes(n)
}

/// Apps not opened for this many calendar months count as "rarely used".
pub const UNUSED_MONTHS: u32 = 6;

/// Text of the three summary tiles.
#[derive(Clone, Debug, PartialEq)]
pub struct Stats {
    pub removable_value: String,
    pub removable_caption: &'static str,
    pub space_value: String,
    pub space_caption: String,
    pub unused_value: String,
    pub unused_caption: String,
    pub unused_any: bool,
}

/// Size of a set of apps: the exact total when all are measured, else a lower
/// bound marked with "+".
fn total_text(sizes: impl Iterator<Item = Option<u64>>) -> (String, usize) {
    let (mut total, mut missing) = (0u64, 0usize);
    for s in sizes {
        match s {
            Some(n) => total += n,
            None => missing += 1,
        }
    }
    (
        if missing == 0 {
            bytes(total)
        } else {
            format!("{}+", bytes(total))
        },
        missing,
    )
}

/// `measuring`: sizes and usage dates are still being gathered.
pub fn stats(apps: &[InstalledApp], measuring: bool, now: SystemTime) -> Stats {
    let removable: Vec<&InstalledApp> = apps.iter().filter(|a| a.is_removable()).collect();
    let unused: Vec<&&InstalledApp> = removable
        .iter()
        .filter(|a| engine::unused_for(a, UNUSED_MONTHS, now))
        .collect();
    let no_data = removable
        .iter()
        .filter(|a| engine::effective_last_used(a, now).is_none())
        .count();

    let (total, missing) = total_text(removable.iter().map(|a| a.size));
    let (unused_total, unused_missing) = total_text(unused.iter().map(|a| a.size));

    let space_caption = if missing > 0 && !measuring {
        format!("By removable apps · {} not measured", missing)
    } else {
        "By removable apps".to_string()
    };
    let unused_caption = if measuring {
        "Measuring…".to_string()
    } else {
        let mut c = format!("{unused_total} could be freed");
        if unused_missing > 0 {
            c.push_str(&format!(" · {unused_missing} not measured"));
        }
        if no_data > 0 {
            c.push_str(&format!(" · {} without usage data", no_data));
        }
        c
    };
    Stats {
        removable_value: removable.len().to_string(),
        removable_caption: "Excludes Apple apps and Nera Cleaner",
        space_value: if measuring {
            "Measuring…".into()
        } else {
            total
        },
        space_caption,
        unused_value: unused.len().to_string(),
        unused_caption,
        unused_any: !unused.is_empty(),
    }
}

/// Toolbar summary of the visible apps: "12 apps · 3.4 GB".
pub fn toolbar_summary(apps: &[InstalledApp], visible: &[usize], measuring: bool) -> String {
    let count = format::count(visible.len(), "app");
    if measuring {
        return count;
    }
    let (total, missing) = total_text(visible.iter().map(|i| apps[*i].size));
    if missing > 0 {
        format!("{count} · {total} ({missing} not measured)")
    } else {
        format!("{count} · {total}")
    }
}

/// Second line of an app row: "Version 1.2 · Used 3 days ago".
pub fn app_subtitle(app: &InstalledApp, measuring: bool, now: SystemTime) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !app.version.is_empty() {
        parts.push(format!("Version {}", app.version));
    }
    let last = engine::effective_last_used(app, now);
    if last.is_some() || !measuring {
        parts.push(engine::describe_last_used(last, now));
    }
    parts.join(" · ")
}

/// Right-hand size of an app row; "…" while it is not measured yet.
pub fn app_size(app: &InstalledApp) -> String {
    app.size.map(format::bytes).unwrap_or_else(|| "…".into())
}

/// Header of the leftovers list.
pub fn leftover_header(checked: usize, checked_size: u64) -> String {
    format!(
        "Leftover files · {checked} selected ({})",
        bytes(checked_size)
    )
}

/// Second line of a leftover row: "Cache · ~/Library/Caches".
pub fn leftover_subtitle(kind: LeftoverKind, parent: &str) -> String {
    format!("{} · {}", kind.label(), parent)
}

/// Note under the leftovers about items shared with other apps.
pub fn shared_note(skipped: usize) -> String {
    format!(
        "{} left out because another installed app uses {}.",
        format::count(skipped, "item"),
        if skipped == 1 { "it" } else { "them" }
    )
}

/// Footer of the selected app.
#[derive(Clone, Debug, PartialEq)]
pub struct Footer {
    pub summary: String,
    pub caption: String,
    pub button: String,
    /// Everything needed to uninstall is known.
    pub ready: bool,
}

pub fn footer(app: &InstalledApp, scanning: bool, checked: usize, checked_size: u64) -> Footer {
    let name = app.name.clone();
    if scanning {
        return Footer {
            summary: name,
            caption: "Looking for leftover files…".into(),
            button: "Uninstall".into(),
            ready: false,
        };
    }
    if app.running {
        let caption = format!("Quit {} to uninstall it.", app.name);
        return Footer {
            summary: name,
            caption,
            button: "Uninstall".into(),
            ready: false,
        };
    }
    let Some(app_size) = app.size else {
        return Footer {
            summary: name,
            caption: "Measuring the app…".into(),
            button: "Uninstall".into(),
            ready: false,
        };
    };
    let total = app_size + checked_size;
    Footer {
        summary: format!("{} · {}", app.name, bytes(total)),
        caption: format!(
            "App {} + {} {}. Moved to the Trash, so you can restore them.",
            bytes(app_size),
            format::count(checked, "leftover item"),
            bytes(checked_size)
        ),
        button: format!("Uninstall {}", bytes(total)),
        ready: true,
    }
}

/// Body of the uninstall confirmation.
pub fn confirm_message(name: &str, app_size: u64, leftovers: usize, leftover_size: u64) -> String {
    let what = if leftovers == 0 {
        format!("{} ({})", name, bytes(app_size))
    } else {
        format!(
            "{} ({}) and {} ({})",
            name,
            bytes(app_size),
            format::count(leftovers, "leftover item"),
            bytes(leftover_size)
        )
    };
    format!(
        "{what} will be moved to the Trash, {} in all. You can restore them from the Trash.",
        bytes(app_size + leftover_size)
    )
}

/// Success message. Moving to the Trash does not free disk space until the
/// Trash is emptied, so this never claims space was freed.
pub fn moved_message(leftovers_removed: usize, moved: u64) -> String {
    let what = if leftovers_removed == 0 {
        "the app".to_string()
    } else {
        format!(
            "the app and {}",
            format::count(leftovers_removed, "leftover item")
        )
    };
    format!(
        "Moved {what} to the Trash ({}). Empty the Trash to free the space.",
        bytes(moved)
    )
}

/// Message when the app moved but some items stayed behind.
pub fn partial_message(moved: u64, first_failure: &str) -> String {
    format!("Moved {} to the Trash. {first_failure}", bytes(moved))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Protection;
    use std::path::PathBuf;
    use std::time::Duration;

    fn app(name: &str, size: Option<u64>, protection: Protection) -> InstalledApp {
        InstalledApp {
            path: PathBuf::from(format!("/Applications/{name}.app")),
            real_path: PathBuf::from(format!("/Applications/{name}.app")),
            executable: None,
            name: name.into(),
            bundle_id: format!("com.example.{name}"),
            version: "1.0".into(),
            alt_names: vec![],
            icon_file: None,
            protection,
            running: false,
            size,
            last_used: None,
            icon_png: None,
        }
    }

    #[test]
    fn stats_count_only_removable_and_label_gaps() {
        let now = SystemTime::now();
        let mut old = app("Old", Some(2_000_000_000), Protection::Removable);
        old.last_used = Some(now - Duration::from_secs(400 * 86_400));
        let mut fresh = app("Fresh", Some(1_000_000_000), Protection::Removable);
        fresh.last_used = Some(now);
        let unknown = app("Unknown", Some(500_000_000), Protection::Removable);
        let apple = app("Mail", Some(9_000_000_000), Protection::Apple);
        let apps = vec![old, fresh, unknown, apple];

        let s = stats(&apps, false, now);
        assert_eq!(s.removable_value, "3");
        assert_eq!(s.space_value, "3.50 GB");
        assert_eq!(s.space_caption, "By removable apps");
        assert_eq!(s.unused_value, "1");
        assert_eq!(
            s.unused_caption,
            "2.00 GB could be freed · 1 without usage data"
        );
        assert!(s.unused_any);

        let m = stats(&apps, true, now);
        assert_eq!(m.space_value, "Measuring…");
        assert_eq!(m.unused_caption, "Measuring…");

        // Unmeasured apps make totals lower bounds, never silent undercounts.
        let mut gap = apps.clone();
        gap[1].size = None;
        let s = stats(&gap, false, now);
        assert_eq!(s.space_value, "2.50 GB+");
        assert_eq!(s.space_caption, "By removable apps · 1 not measured");
        assert_eq!(
            toolbar_summary(&gap, &[0, 1, 2], false),
            "3 apps · 2.50 GB+ (1 not measured)"
        );
        assert_eq!(
            toolbar_summary(&apps, &[0, 1, 2], false),
            "3 apps · 3.50 GB"
        );
        assert_eq!(toolbar_summary(&gap, &[0], true), "1 app");
    }

    #[test]
    fn bytes_round_before_choosing_the_unit() {
        assert_eq!(bytes(999), "999 bytes");
        assert_eq!(bytes(999_499), "999 KB");
        assert_eq!(bytes(999_500), "1.00 MB");
        assert_eq!(bytes(9_996_000), "10.0 MB");
        assert_eq!(bytes(99_950_000), "100 MB");
        assert_eq!(bytes(999_600_000), "1.00 GB");
        assert_eq!(bytes(3_270_000_000), "3.27 GB");
        assert_eq!(bytes(227_569_664), "228 MB");
        for n in [1_234u64, 56_789, 7_654_321, 87_654_321, 4_567_890_123] {
            assert_eq!(bytes(n), format::bytes(n), "{n}");
        }
    }

    #[test]
    fn running_app_is_in_use_now() {
        let now = SystemTime::now();
        let mut a = app("Run", Some(1), Protection::Removable);
        a.last_used = Some(now - Duration::from_secs(900 * 86_400));
        assert!(engine::unused_for(&a, 6, now));
        assert_eq!(
            app_subtitle(&a, false, now),
            "Version 1.0 · Used 2 years ago"
        );
        a.running = true;
        assert!(!engine::unused_for(&a, 6, now));
        assert_eq!(app_subtitle(&a, false, now), "Version 1.0 · Used today");
        a.version.clear();
        a.running = false;
        a.last_used = None;
        assert_eq!(app_subtitle(&a, true, now), "");
        assert_eq!(app_subtitle(&a, false, now), "No usage data");
    }

    #[test]
    fn footer_and_messages() {
        let mut a = app("Slack", Some(300_000_000), Protection::Removable);
        let f = footer(&a, false, 2, 20_000_000);
        assert_eq!(f.summary, "Slack · 320 MB");
        assert_eq!(
            f.caption,
            "App 300 MB + 2 leftover items 20.0 MB. Moved to the Trash, so you can restore them."
        );
        assert_eq!(f.button, "Uninstall 320 MB");
        assert!(f.ready);
        assert!(!footer(&a, true, 0, 0).ready);
        a.running = true;
        assert!(!footer(&a, false, 0, 0).ready);
        a.running = false;
        a.size = None;
        let f = footer(&a, false, 0, 0);
        assert!(!f.ready);
        assert_eq!(f.button, "Uninstall");

        assert_eq!(
            confirm_message("Slack", 300_000_000, 2, 20_000_000),
            "Slack (300 MB) and 2 leftover items (20.0 MB) will be moved to the Trash, 320 MB in all. You can restore them from the Trash."
        );
        assert_eq!(
            confirm_message("Slack", 300_000_000, 0, 0),
            "Slack (300 MB) will be moved to the Trash, 300 MB in all. You can restore them from the Trash."
        );
        assert_eq!(
            moved_message(0, 300_000_000),
            "Moved the app to the Trash (300 MB). Empty the Trash to free the space."
        );
        assert_eq!(
            moved_message(1, 320_000_000),
            "Moved the app and 1 leftover item to the Trash (320 MB). Empty the Trash to free the space."
        );
        assert_eq!(
            leftover_header(2, 20_000_000),
            "Leftover files · 2 selected (20.0 MB)"
        );
        assert_eq!(
            leftover_subtitle(LeftoverKind::Caches, "~/Library/Caches"),
            "Cache · ~/Library/Caches"
        );
        assert_eq!(
            shared_note(1),
            "1 item left out because another installed app uses it."
        );
        assert_eq!(
            shared_note(3),
            "3 items left out because another installed app uses them."
        );
    }
}
