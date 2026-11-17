//! System junk engine: categories, scanning, overlap handling and cleaning.
//! Port of ScanTargets / ScannerActor / CategoryPathOverlap /
//! DeleteFilesUseCase. No UI code; every function here is blocking and is
//! called from `nera_ui::run_blocking`.

mod clean;
#[cfg(target_os = "macos")]
mod cloud;
mod items;
mod overlap;
mod plan;
mod scan;
#[cfg(target_os = "macos")]
mod size;
#[cfg(windows)]
#[path = "engine/size_windows.rs"]
mod size;
#[cfg(target_os = "macos")]
mod system_assets;
mod targets;
/// Windows primitives `nera-core` does not offer (yet).
#[cfg(windows)]
pub(crate) mod win;

pub use clean::{clean, in_use_sentence, CategoryOutcome, CleanReport, Status};
#[cfg(target_os = "macos")]
pub use cloud::{cloud_drives, CloudDrive};
pub use items::{removal_for, removal_items};
pub use overlap::{cleaning_order, reserved_paths, PathIndex};
pub use plan::{
    auto_selection, blocked_categories, empty_names, grouped, safe_selection, Disposal,
};
pub use scan::{old_installers, rescan, scan, Options, ScanOutcome, Scanned};
pub use size::{item_bytes, Removal};
#[cfg(target_os = "macos")]
pub use system_assets::{system_assets, Feature, SystemAsset};
pub use targets::{all_categories, Category, Group, Mode};

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Progress of a scan or cleanup, written from background work.
#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    /// Category being processed (empty before the first one).
    pub current: String,
}

impl Progress {
    pub fn fraction(&self) -> Option<f64> {
        (self.total > 0).then(|| self.done as f64 / self.total as f64)
    }
}

/// Smallest size worth auto-selecting after a scan (1 MB).
pub const AUTO_SELECT_MIN: u64 = 1_000_000;

/// A scan that finds less than this on a real Mac usually means Full Disk
/// Access is missing.
pub const SUSPICIOUSLY_SMALL: u64 = 100_000_000;

// MARK: - Full Disk Access

/// Probe TCC-protected locations. `Some(true)` granted, `Some(false)` denied,
/// `None` when no probe location exists. Read-only.
#[cfg(target_os = "macos")]
pub fn full_disk_access() -> Option<bool> {
    full_disk_access_in(&nera_core::paths::home())
}

/// Windows has no Full Disk Access: a folder is readable, or it needs an
/// administrator (reported per category, see `present::notes`).
#[cfg(windows)]
pub fn full_disk_access() -> Option<bool> {
    None
}

#[cfg(target_os = "macos")]
fn full_disk_access_in(home: &Path) -> Option<bool> {
    let mut denied = false;
    for rel in [
        "Library/Safari",
        "Library/Mail",
        "Library/Application Support/com.apple.TCC",
        "Library/Messages",
        "Library/Cookies",
    ] {
        let dir = home.join(rel);
        if std::fs::symlink_metadata(&dir).is_err() {
            continue;
        }
        match std::fs::read_dir(&dir) {
            Ok(mut entries) => {
                if entries.next().is_some() {
                    return Some(true);
                }
            }
            Err(_) => denied = true,
        }
    }
    for rel in [
        "Library/Safari/Bookmarks.plist",
        "Library/Safari/CloudTabs.db",
        "Library/Cookies/Cookies.binarycookies",
        "Library/Mail/V10/MailData/Envelope Index",
    ] {
        let file = home.join(rel);
        if std::fs::symlink_metadata(&file).is_err() {
            continue;
        }
        if std::fs::File::open(&file).is_ok() {
            return Some(true);
        }
        denied = true;
    }
    denied.then_some(false)
}

/// System Settings › Privacy & Security › Full Disk Access.
pub const FULL_DISK_ACCESS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";

// MARK: - Last cleanup (persisted)

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastCleanup {
    pub bytes: u64,
    pub categories: usize,
    /// Seconds since the Unix epoch.
    pub at: u64,
    /// The part of `bytes` that was moved to the Trash (freed once it is emptied).
    #[serde(default)]
    pub trashed: u64,
}

impl LastCleanup {
    pub fn now(bytes: u64, categories: usize, trashed: u64) -> Self {
        Self {
            bytes,
            categories,
            at: unix_now(),
            trashed,
        }
    }

    fn file() -> PathBuf {
        nera_core::paths::app_support_dir().join("last-cleanup.json")
    }

    pub fn load() -> Option<Self> {
        Self::load_from(&Self::file())
    }

    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&Self::file())
    }

    pub fn load_from(path: &Path) -> Option<Self> {
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        nera_core::fs_util::write_atomic(path, &serde_json::to_vec(self)?)?;
        Ok(())
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// "Just now", "5 minutes ago", "3 hours ago", "2 days ago".
pub fn relative_time(at: u64, now: u64) -> String {
    let secs = now.saturating_sub(at);
    let plural = |n: u64, unit: &str| {
        if n == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{n} {unit}s ago")
        }
    };
    match secs {
        0..=59 => "Just now".into(),
        60..=3599 => plural(secs / 60, "minute"),
        3600..=86_399 => plural(secs / 3600, "hour"),
        _ => plural(secs / 86_400, "day"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn full_disk_access_probe_is_read_only_and_answers() {
        // Real home: any answer is fine, it must not panic.
        let _ = full_disk_access();
        let empty = std::env::temp_dir().join(format!("nera-fda-{}", std::process::id()));
        std::fs::create_dir_all(empty.join("Library/Safari")).unwrap();
        std::fs::write(empty.join("Library/Safari/Bookmarks.plist"), b"x").unwrap();
        assert_eq!(full_disk_access_in(&empty), Some(true));
        std::fs::remove_dir_all(&empty).unwrap();
        assert_eq!(full_disk_access_in(&empty), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_has_no_full_disk_access() {
        assert_eq!(full_disk_access(), None);
    }

    #[test]
    fn last_cleanup_round_trips() {
        let dir = std::env::temp_dir().join(format!("nera-last-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("last.json");
        assert!(LastCleanup::load_from(&file).is_none());
        let last = LastCleanup {
            bytes: 42,
            categories: 3,
            at: 1000,
            trashed: 40,
        };
        last.save_to(&file).unwrap();
        assert_eq!(LastCleanup::load_from(&file), Some(last));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn old_last_cleanup_files_still_load() {
        let last: LastCleanup =
            serde_json::from_str(r#"{"bytes":1,"categories":2,"at":3}"#).unwrap();
        assert_eq!(last.trashed, 0);
    }

    #[test]
    fn formats_relative_time() {
        assert_eq!(relative_time(100, 130), "Just now");
        assert_eq!(relative_time(0, 60), "1 minute ago");
        assert_eq!(relative_time(0, 7200), "2 hours ago");
        assert_eq!(relative_time(0, 3 * 86_400), "3 days ago");
        assert_eq!(
            Progress {
                done: 1,
                total: 4,
                current: String::new()
            }
            .fraction(),
            Some(0.25)
        );
    }
}

/// Windows: cloud placeholders are not measured (yet), so nothing is shown.
#[cfg(not(target_os = "macos"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudDrive {
    pub service: String,
    pub account: Option<String>,
    pub path: PathBuf,
    pub local_bytes: u64,
    pub local_files: u64,
}

/// Windows has no counterpart to the macOS system assets.
#[cfg(not(target_os = "macos"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {}

#[cfg(not(target_os = "macos"))]
impl Feature {
    pub fn name(self) -> &'static str {
        match self {}
    }
    pub fn detail(self) -> &'static str {
        match self {}
    }
    pub fn settings_url(self) -> &'static str {
        match self {}
    }
}

#[cfg(not(target_os = "macos"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemAsset {
    pub feature: Feature,
    pub bytes: u64,
}

#[cfg(not(target_os = "macos"))]
pub fn system_assets(_cancel: &nera_core::fs_util::Cancel) -> Vec<SystemAsset> {
    Vec::new()
}

#[cfg(not(target_os = "macos"))]
pub fn cloud_drives(_cancel: &nera_core::fs_util::Cancel) -> Vec<CloudDrive> {
    Vec::new()
}
