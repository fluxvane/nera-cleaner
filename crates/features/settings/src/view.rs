//! Every value and label the Settings screen shows, as pure functions of the
//! persisted [`Settings`] and the engine's measurements. The screen renders
//! these verbatim and `examples/dump.rs` prints them.

use crate::engine::{self, AccessReport, FullDiskAccess, MAX_SCAN_DEPTH, MIN_SCAN_DEPTH};
use nera_core::format;
use nera_core::settings::Settings;
use std::path::Path;

pub const MOVE_TO_TRASH_TITLE: &str = concat!("Move to ", nera_core::trash_name!());
pub const MOVE_TO_TRASH_DESC: &str = concat!(
    "Cleaned files go to the ",
    nera_core::trash_name!(),
    ". Private browser data and the ",
    nera_core::trash_name!(),
    " itself are always deleted for good."
);
pub const THEME_DESC: &str = concat!(
    "System follows your ",
    nera_core::os_name!(),
    " appearance."
);
pub const TAGLINE: &str = concat!(
    "Free up disk space and keep your ",
    nera_core::computer!(),
    " running smoothly."
);
pub const CONFIRM_DESC: &str = "Ask before a cleanup starts, with a summary of what goes.";
pub const NOTIFY_DESC: &str = concat!(
    "Show a ",
    nera_core::os_name!(),
    " notification when a cleanup finishes."
);
pub const HIDDEN_DESC: &str =
    "Cleanup and Duplicates also look at files and folders whose names start with a dot.";
pub const DEPTH_DESC: &str = "How many folder levels deep Cleanup size scans go (1 to 50).";
pub const LAUNCH_DESC: &str = "Run Smart Scan automatically when Nera Cleaner opens.";
pub const EXCLUDED_DESC: &str =
    "Cleanup, Dev scan and Duplicates skip these folders and everything inside them.";
pub const COPYRIGHT: &str = "\u{a9} Nextera. All rights reserved.";

/// Section description under "Cleanup".
pub fn cleanup_description(move_to_trash: bool) -> &'static str {
    if move_to_trash {
        concat!(
            "Most cleaned files can be restored from the ",
            nera_core::trash_name!(),
            "."
        )
    } else {
        concat!(
            "Cleaned files are deleted immediately and can't be restored. A few items always go to the ",
            nera_core::trash_name!(),
            "."
        )
    }
}

/// The scan depth the app actually uses (stored values outside 1 to 50 are clamped).
pub fn depth(settings: &Settings) -> u32 {
    settings
        .max_scan_depth
        .clamp(MIN_SCAN_DEPTH, MAX_SCAN_DEPTH)
}

/// Title and subtitle of an excluded-folder row.
pub fn excluded_row(path: &Path) -> (String, String) {
    (engine::folder_name(path), engine::display_path(path))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionTone {
    Neutral,
    Success,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Permission {
    pub badge: &'static str,
    pub tone: PermissionTone,
    pub subtitle: String,
}

/// `["Safari", "Mail"]` -> `"Safari and Mail"`.
fn names(list: &[&str]) -> String {
    match list {
        [] => String::new(),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The Full Disk Access row. It reports what was actually tested.
pub fn permission(report: Option<&AccessReport>, checking: bool) -> Permission {
    let make = |badge, tone, subtitle: String| Permission {
        badge,
        tone,
        subtitle,
    };
    if checking {
        return make(
            "Checking\u{2026}",
            PermissionTone::Neutral,
            "Looking for access to protected folders.".into(),
        );
    }
    match report {
        None => make(
            "Unknown",
            PermissionTone::Neutral,
            "Access has not been checked yet.".into(),
        ),
        Some(r) => match r.status {
            FullDiskAccess::Granted => make(
                "Granted",
                PermissionTone::Success,
                format!(
                    "Nera Cleaner could read {}, which macOS protects.",
                    names(&r.readable)
                ),
            ),
            FullDiskAccess::Denied => make(
                "Not granted",
                PermissionTone::Warning,
                format!(
                    "macOS blocked access to {}. Those folders are skipped during scans.",
                    names(&r.denied)
                ),
            ),
            FullDiskAccess::Unknown => make(
                "Unknown",
                PermissionTone::Neutral,
                "No protected folder was found to test access with.".into(),
            ),
        },
    }
}

/// Value of the cache row.
pub fn storage_value(bytes: Option<u64>) -> String {
    match bytes {
        Some(b) => format::bytes(b),
        None => "Calculating\u{2026}".to_string(),
    }
}

pub fn version_text() -> &'static str {
    nera_core::version()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(
        status: FullDiskAccess,
        readable: &[&'static str],
        denied: &[&'static str],
    ) -> AccessReport {
        AccessReport {
            status,
            readable: readable.to_vec(),
            denied: denied.to_vec(),
        }
    }

    #[test]
    fn permission_row_says_what_was_tested() {
        let granted = permission(
            Some(&report(
                FullDiskAccess::Granted,
                &["Safari", "Mail", "Messages"],
                &[],
            )),
            false,
        );
        assert_eq!(
            (granted.badge, granted.tone),
            ("Granted", PermissionTone::Success)
        );
        assert_eq!(
            granted.subtitle,
            "Nera Cleaner could read Safari, Mail and Messages, which macOS protects."
        );
        let denied = permission(
            Some(&report(FullDiskAccess::Denied, &[], &["Safari"])),
            false,
        );
        assert_eq!(
            (denied.badge, denied.tone),
            ("Not granted", PermissionTone::Warning)
        );
        assert_eq!(
            denied.subtitle,
            "macOS blocked access to Safari. Those folders are skipped during scans."
        );
        let unknown = permission(Some(&report(FullDiskAccess::Unknown, &[], &[])), false);
        assert_eq!(unknown.badge, "Unknown");
        assert_eq!(permission(None, false).badge, "Unknown");
        assert_eq!(permission(None, true).badge, "Checking\u{2026}");
    }

    #[test]
    fn depth_shown_is_the_depth_used() {
        let mut s = Settings::default();
        assert_eq!(depth(&s), 15);
        s.max_scan_depth = 500;
        assert_eq!(depth(&s), 50);
        assert_eq!(depth(&s) as usize, s.scan_depth());
        s.max_scan_depth = 0;
        assert_eq!(depth(&s), 1);
        assert_eq!(depth(&s) as usize, s.scan_depth());
    }

    #[test]
    fn storage_and_version() {
        assert_eq!(storage_value(None), "Calculating\u{2026}");
        assert_eq!(storage_value(Some(0)), "0 bytes");
        assert_eq!(storage_value(Some(1_976_000)), "1.98 MB");
        assert_eq!(version_text(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn excluded_rows_show_the_folder_and_where_it_is() {
        let path = nera_core::paths::home().join("Code").join("app");
        let (title, subtitle) = excluded_row(&path);
        assert_eq!(title, "app");
        #[cfg(target_os = "macos")]
        assert_eq!(subtitle, "~/Code/app");
        #[cfg(windows)]
        assert_eq!(subtitle, path.display().to_string());
    }

    #[test]
    fn cleanup_description_does_not_overpromise() {
        assert!(cleanup_description(true).starts_with("Most"));
        assert!(cleanup_description(false)
            .contains(concat!("always go to the ", nera_core::trash_name!())));
    }

    #[test]
    fn displayed_settings_round_trip_through_the_file_format() {
        let s = Settings {
            theme: nera_core::settings::ThemeMode::Dark,
            move_to_trash: false,
            max_scan_depth: 7,
            excluded_paths: vec!["/tmp/a".into(), "/tmp/b".into()],
            ..Settings::default()
        };
        let json = serde_json::to_vec_pretty(&s).unwrap();
        let back: Settings = serde_json::from_slice(&json).unwrap();
        assert_eq!(
            back, s,
            "what the screen shows after a restart is what was saved"
        );
    }
}
