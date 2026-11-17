//! Pure text shown by the Applications screen on Windows. Every number and
//! label the screen displays is produced here so it can be unit-tested and
//! printed by `examples/dump_apps.rs` exactly as the UI shows it.

use crate::engine::{self, InstalledApp, Protection};
use nera_core::format;

/// A size as Settings > Apps shows it: binary units (1 KB = 1024 bytes) with
/// three significant digits, rounded ("7.72 MB", "68.0 KB", "1.19 GB").
pub fn size_text(n: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if n < 1024 {
        return if n == 1 {
            "1 byte".into()
        } else {
            format!("{n} bytes")
        };
    }
    let decimals = |v: f64| -> usize {
        if v >= 100.0 {
            0
        } else if v >= 10.0 {
            1
        } else {
            2
        }
    };
    let (mut value, mut unit) = (n as f64 / 1024.0, 0usize);
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    loop {
        let factor = 10f64.powi(decimals(value) as i32);
        let rounded = (value * factor).round() / factor;
        if rounded >= 1000.0 && unit < UNITS.len() - 1 {
            value = rounded / 1024.0;
            unit += 1;
            continue;
        }
        return format!("{rounded:.*} {}", decimals(rounded), UNITS[unit]);
    }
}

/// Text of the three summary tiles.
#[derive(Clone, Debug, PartialEq)]
pub struct Stats {
    pub apps_value: String,
    pub apps_caption: &'static str,
    pub space_value: String,
    pub space_caption: String,
    pub running_value: String,
    pub running_caption: &'static str,
    pub running_any: bool,
}

/// Total of the sizes Windows reports, marked with "+" when some app reports
/// none (so the total is a lower bound), and how many report none.
fn total_text(sizes: impl Iterator<Item = Option<u64>>) -> (String, usize) {
    let (mut total, mut missing) = (0u64, 0usize);
    for s in sizes {
        match s {
            Some(n) => total += n,
            None => missing += 1,
        }
    }
    let text = if missing == 0 {
        size_text(total)
    } else {
        format!("{}+", size_text(total))
    };
    (text, missing)
}

pub fn stats(apps: &[InstalledApp]) -> Stats {
    let (space, missing) = total_text(apps.iter().map(|a| a.size));
    let running = apps.iter().filter(|a| a.running).count();
    Stats {
        apps_value: apps.len().to_string(),
        apps_caption: "Desktop programs · Store apps not listed",
        space_value: space,
        space_caption: if missing > 0 {
            format!(
                "Sizes reported to Windows · {} without one",
                format::count(missing, "app")
            )
        } else {
            "Sizes reported to Windows".into()
        },
        running_value: running.to_string(),
        running_caption: "Found by their install folder",
        running_any: running > 0,
    }
}

/// Toolbar summary of the visible apps: "12 apps · 3.40 GB".
pub fn toolbar_summary(apps: &[InstalledApp], visible: &[usize]) -> String {
    let count = format::count(visible.len(), "app");
    let (total, missing) = total_text(visible.iter().map(|i| apps[*i].size));
    if missing > 0 {
        format!("{count} · {total} ({missing} without size)")
    } else {
        format!("{count} · {total}")
    }
}

/// Second line of an app row: "Version 1.2 · Publisher · Installed 5/13/2026".
pub fn app_subtitle(app: &InstalledApp) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !app.version.is_empty() {
        parts.push(format!("Version {}", app.version));
    }
    if !app.publisher.is_empty() {
        parts.push(app.publisher.clone());
    }
    if let Some(date) = app.install_date {
        parts.push(format!("Installed {}", engine::format_date(date)));
    }
    parts.join(" · ")
}

/// Right-hand size of an app row; empty when the installer reports none, as in Settings.
pub fn app_size(app: &InstalledApp) -> String {
    app.size.map(size_text).unwrap_or_default()
}

/// Badge shown when an app has no uninstall action here, if any.
pub fn blocker_badge(app: &InstalledApp) -> Option<&'static str> {
    if app.protection == Protection::SelfApp {
        Some("This app")
    } else if app.no_remove {
        Some("Can't be removed")
    } else if app.uninstall.is_none() {
        Some("No uninstaller")
    } else {
        None
    }
}

/// Note at the bottom of the selected app's details.
pub const LEFTOVERS_NOTE: &str = "Leftover files are not listed on Windows: data apps keep in AppData can't be matched to an app reliably.";

/// Footer of the selected app.
#[derive(Clone, Debug, PartialEq)]
pub struct Footer {
    pub summary: String,
    pub caption: String,
    pub button: &'static str,
    /// The uninstaller can be opened now.
    pub ready: bool,
}

/// `waiting`: an uninstaller opened from Nera Cleaner is still at work.
pub fn footer(app: &InstalledApp, waiting: bool) -> Footer {
    let summary = match app.size {
        Some(size) => format!("{} · {}", app.name, size_text(size)),
        None => app.name.clone(),
    };
    let caption = if waiting {
        "Wait for the uninstaller that is open to finish.".to_string()
    } else if app.running {
        format!(
            "{} is running. Its uninstaller may ask you to close it.",
            app.name
        )
    } else {
        "Opens the app's own uninstaller. Nera Cleaner doesn't delete anything itself.".to_string()
    };
    Footer {
        summary,
        caption,
        button: "Uninstall",
        ready: app.can_uninstall() && !waiting,
    }
}

/// Uninstall confirmation: title, body and button.
pub fn confirm_text(name: &str) -> (String, String, &'static str) {
    (
        format!("Uninstall {name}?"),
        format!(
            "{name}'s own uninstaller will open. Follow its steps to remove the app; Windows may ask for permission first. Nera Cleaner doesn't delete any files itself."
        ),
        "Open uninstaller",
    )
}

/// Banner while the uninstaller runs: title and message.
pub fn waiting_text(name: &str) -> (String, &'static str) {
    (
        format!("Waiting for {name}'s uninstaller"),
        "Finish the steps in its window. This list updates when Windows no longer lists the app.",
    )
}

/// The app is gone from the registry: title and message.
pub fn uninstalled_text(name: &str) -> (String, &'static str) {
    (
        format!("Uninstalled {name}"),
        "Windows no longer lists it under Installed apps.",
    )
}

/// Waited long enough and the app is still listed: title and message.
pub fn still_listed_text(name: &str) -> (String, &'static str) {
    (
        format!("{name} is still installed"),
        "Its uninstaller finished or was cancelled, but Windows still lists the app.",
    )
}

/// Why the uninstaller did not open: title and message.
pub fn launch_failed_text(name: &str, err: &engine::LaunchError) -> (String, String) {
    match err {
        engine::LaunchError::Cancelled => (
            "Uninstall cancelled".into(),
            "Windows didn't get permission to run the uninstaller. Nothing changed.".into(),
        ),
        engine::LaunchError::NotFound(program) => (
            format!("Couldn't open {name}'s uninstaller"),
            format!("Windows can't find {program}. Nothing changed."),
        ),
        engine::LaunchError::Other(reason) => (
            format!("Couldn't open {name}'s uninstaller"),
            format!("{reason} Nothing changed."),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Scope, UninstallCommand};
    use chrono::NaiveDate;

    fn app(name: &str, size: Option<u64>) -> InstalledApp {
        InstalledApp {
            id: format!("HKCU\\{name}"),
            scope: Scope::User,
            key: name.into(),
            name: name.into(),
            version: "1.0".into(),
            publisher: "Example Inc.".into(),
            install_location: None,
            app_folder: None,
            size,
            install_date: None,
            uninstall: Some(UninstallCommand {
                program: r"C:\Program Files\X\unins000.exe".into(),
                args: String::new(),
            }),
            no_remove: false,
            protection: Protection::Removable,
            running: false,
        }
    }

    #[test]
    fn sizes_match_settings() {
        // Values Settings > Apps showed for these exact byte counts.
        assert_eq!(size_text(8_092_160), "7.72 MB"); // 7-Zip, rounded not truncated
        assert_eq!(size_text(55_272_448), "52.7 MB"); // GitHub CLI
        assert_eq!(size_text(790_528), "772 KB"); // vs_CoreEditorFonts
        assert_eq!(size_text(2_140_160), "2.04 MB"); // Microsoft Update Health Tools
        assert_eq!(size_text(117_760 * 1024), "115 MB"); // Discord
        assert_eq!(size_text(1_248_946 * 1024), "1.19 GB"); // Bitwarden
        assert_eq!(size_text(9_147_517 * 1024), "8.72 GB"); // Swift toolkit
        assert_eq!(size_text(100_692 * 1024), "98.3 MB"); // .NET runtime
        assert_eq!(size_text(371 * 1024), "371 KB"); // Mozilla Maintenance Service
        assert_eq!(size_text(68 * 1024), "68.0 KB");
        assert_eq!(size_text(8 * 1024), "8.00 KB");
        assert_eq!(size_text(1), "1 byte");
        assert_eq!(size_text(1000), "1000 bytes");
        // Rounding across a unit boundary moves to the next unit.
        assert_eq!(size_text(1_048_575), "1.00 MB");
    }

    #[test]
    fn stats_and_summary() {
        let mut a = app("A", Some(2 * 1024 * 1024 * 1024));
        a.running = true;
        let b = app("B", Some(1024 * 1024 * 1024));
        let c = app("C", None);
        let apps = vec![a, b, c];
        let s = stats(&apps);
        assert_eq!(s.apps_value, "3");
        assert_eq!(s.space_value, "3.00 GB+");
        assert_eq!(
            s.space_caption,
            "Sizes reported to Windows · 1 app without one"
        );
        assert_eq!(s.running_value, "1");
        assert!(s.running_any);
        assert_eq!(
            toolbar_summary(&apps, &[0, 1, 2]),
            "3 apps · 3.00 GB+ (1 without size)"
        );
        assert_eq!(toolbar_summary(&apps, &[1]), "1 app · 1.00 GB");
        assert_eq!(stats(&apps[..2]).space_caption, "Sizes reported to Windows");
    }

    #[test]
    fn rows_badges_and_footer() {
        let mut a = app("Foo", Some(300 * 1024 * 1024));
        assert_eq!(app_subtitle(&a), "Version 1.0 · Example Inc.");
        let date = NaiveDate::from_ymd_opt(2026, 5, 13).unwrap();
        a.install_date = Some(date);
        assert_eq!(
            app_subtitle(&a),
            format!(
                "Version 1.0 · Example Inc. · Installed {}",
                engine::format_date(date)
            )
        );
        assert_eq!(app_size(&a), "300 MB");
        assert_eq!(app_size(&app("X", None)), "");
        assert_eq!(blocker_badge(&a), None);

        let f = footer(&a, false);
        assert_eq!(f.summary, "Foo · 300 MB");
        assert!(f.ready);
        assert!(!footer(&a, true).ready);
        a.running = true;
        let f = footer(&a, false);
        assert!(
            f.ready,
            "a running app can still be handed to its uninstaller"
        );
        assert_eq!(
            f.caption,
            "Foo is running. Its uninstaller may ask you to close it."
        );

        a.no_remove = true;
        assert_eq!(blocker_badge(&a), Some("Can't be removed"));
        assert!(!footer(&a, false).ready);
        a.no_remove = false;
        a.uninstall = None;
        assert_eq!(blocker_badge(&a), Some("No uninstaller"));
        a.protection = Protection::SelfApp;
        assert_eq!(blocker_badge(&a), Some("This app"));
    }

    #[test]
    fn messages() {
        let (title, body, button) = confirm_text("Slack");
        assert_eq!(title, "Uninstall Slack?");
        assert!(body.starts_with("Slack's own uninstaller will open."));
        assert_eq!(button, "Open uninstaller");
        assert_eq!(uninstalled_text("Slack").0, "Uninstalled Slack");
        assert_eq!(still_listed_text("Slack").0, "Slack is still installed");
        assert_eq!(waiting_text("Slack").0, "Waiting for Slack's uninstaller");
        let (t, m) = launch_failed_text("Slack", &engine::LaunchError::Cancelled);
        assert_eq!(t, "Uninstall cancelled");
        assert!(m.ends_with("Nothing changed."));
        let (t, m) =
            launch_failed_text("Slack", &engine::LaunchError::NotFound(r"C:\x.exe".into()));
        assert_eq!(t, "Couldn't open Slack's uninstaller");
        assert_eq!(m, r"Windows can't find C:\x.exe. Nothing changed.");
    }
}
