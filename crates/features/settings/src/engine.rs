//! Settings engine: Full Disk Access detection, excluded-folder bookkeeping,
//! scan-depth stepping and the app's own cache size. No UI; unit-tested.
//!
//! Port of `SettingsViewModel.swift` (cache size) extended with Full Disk
//! Access detection and exclusion-list rules. Nothing here deletes anything.

use nera_core::fs_util::{self, Cancel, SizeOptions};
use nera_core::paths;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub const MIN_SCAN_DEPTH: u32 = 1;
pub const MAX_SCAN_DEPTH: u32 = 50;

/// Deep link to Privacy & Security > Full Disk Access.
#[cfg(target_os = "macos")]
pub const FULL_DISK_ACCESS_PANE: &str = fs_util::FULL_DISK_ACCESS_PANE;

/// Whether the platform gates folders behind a Full Disk Access permission.
/// Windows has no equivalent, so the Permissions section is hidden there.
pub const HAS_FULL_DISK_ACCESS: bool = cfg!(target_os = "macos");

// ---------------------------------------------------------------------------
// Full Disk Access
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FullDiskAccess {
    Granted,
    Denied,
    /// None of the protected probe folders exist, so access cannot be tested.
    Unknown,
}

/// Folders macOS protects behind Full Disk Access: (path relative to home, name shown).
#[cfg(target_os = "macos")]
const PROBES: [(&str, &str); 5] = [
    ("Library/Safari", "Safari"),
    ("Library/Mail", "Mail"),
    ("Library/Messages", "Messages"),
    (
        "Library/Application Support/com.apple.TCC",
        "the privacy database",
    ),
    ("Library/Cookies", "Cookies"),
];
#[cfg(not(target_os = "macos"))]
const PROBES: [(&str, &str); 0] = [];

/// What the probe found: the verdict and the folders behind it, so the screen
/// can say exactly what was tested.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessReport {
    pub status: FullDiskAccess,
    /// Protected folders that could be listed.
    pub readable: Vec<&'static str>,
    /// Protected folders macOS refused to list.
    pub denied: Vec<&'static str>,
}

/// Check whether this process can read protected folders (read-only).
pub fn check_full_disk_access() -> AccessReport {
    let home = paths::home();
    probe_access(PROBES.iter().map(|(rel, name)| (home.join(rel), *name)))
}

/// Verdict for a list of protected folders: any denied => Denied, otherwise any
/// folder with something in it that could be listed => Granted, otherwise
/// (none exist, or only empty ones) Unknown. An empty listing proves nothing,
/// so this is the same rule the Privacy screen uses.
fn probe_access(folders: impl IntoIterator<Item = (PathBuf, &'static str)>) -> AccessReport {
    let (mut readable, mut denied) = (Vec::new(), Vec::new());
    for (folder, name) in folders {
        match std::fs::read_dir(&folder) {
            Ok(mut entries) => {
                if entries.next().is_some() {
                    readable.push(name);
                }
            }
            Err(e) if e.kind() == ErrorKind::PermissionDenied => denied.push(name),
            Err(_) => {}
        }
    }
    let status = if !denied.is_empty() {
        FullDiskAccess::Denied
    } else if !readable.is_empty() {
        FullDiskAccess::Granted
    } else {
        FullDiskAccess::Unknown
    };
    AccessReport {
        status,
        readable,
        denied,
    }
}

// ---------------------------------------------------------------------------
// Scan depth
// ---------------------------------------------------------------------------

/// Move the depth by `delta`, staying within 1...50.
pub fn step_depth(current: u32, delta: i32) -> u32 {
    (current as i64 + delta as i64).clamp(MIN_SCAN_DEPTH as i64, MAX_SCAN_DEPTH as i64) as u32
}

// ---------------------------------------------------------------------------
// Excluded folders
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddExcluded {
    Added(PathBuf),
    /// Already in the list, or inside a folder that is.
    AlreadyCovered(PathBuf),
    /// Not a folder, or too broad to exclude (the whole disk).
    Rejected(String),
}

/// Validate `picked` and add it to `list` (keeping `list` sorted-by-insertion,
/// deduplicated). A new folder that contains existing entries replaces them.
pub fn add_excluded(list: &mut Vec<PathBuf>, picked: &Path) -> AddExcluded {
    let path = paths::standardize(picked);
    if path.parent().is_none() {
        return AddExcluded::Rejected("The whole disk cannot be excluded.".into());
    }
    if !picked.is_dir() {
        return AddExcluded::Rejected(format!("{} is not a folder.", display_path(picked)));
    }
    if list.iter().any(|existing| paths::is_under(&path, existing)) {
        return AddExcluded::AlreadyCovered(path);
    }
    list.retain(|existing| !paths::is_under(existing, &path));
    list.push(path.clone());
    AddExcluded::Added(path)
}

/// Remove `path` from the list; true when something was removed.
pub fn remove_excluded(list: &mut Vec<PathBuf>, path: &Path) -> bool {
    let before = list.len();
    list.retain(|p| p != path);
    list.len() != before
}

/// Path for display: `~`-abbreviated on macOS, in full on Windows (where `~`
/// is not a convention users know).
pub fn display_path(path: &Path) -> String {
    if cfg!(windows) {
        return path.display().to_string();
    }
    let home = paths::home();
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Folder name for a row title (falls back to the full path).
pub fn folder_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

// ---------------------------------------------------------------------------
// Nera's own cache
// ---------------------------------------------------------------------------

pub fn app_cache_dir() -> PathBuf {
    fs_util::app_cache_dir()
}

/// Allocated size of Nera's cache folder (0 when it does not exist).
pub fn app_cache_size() -> u64 {
    fs_util::dir_size(&app_cache_dir(), &SizeOptions::default(), &Cancel::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nera-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn depth_is_clamped() {
        assert_eq!(step_depth(15, 1), 16);
        assert_eq!(step_depth(1, -1), 1);
        assert_eq!(step_depth(50, 1), 50);
        assert_eq!(step_depth(48, 5), 50);
        assert_eq!(step_depth(3, -10), 1);
    }

    #[test]
    fn whole_disk_cannot_be_excluded() {
        let mut list = Vec::new();
        for root in ["/", "C:\\"] {
            assert!(matches!(
                add_excluded(&mut list, Path::new(root)),
                AddExcluded::Rejected(_)
            ));
        }
        assert!(list.is_empty());
    }

    #[test]
    fn probe_reports_denied_granted_unknown() {
        let readable = temp("probe");
        std::fs::write(readable.join("item"), b"x").unwrap();
        let empty = readable.join("empty");
        std::fs::create_dir(&empty).unwrap();
        let missing = readable.join("nope");
        // An empty folder is inconclusive, not proof of access.
        assert_eq!(
            probe_access([(empty, "Empty")]).status,
            FullDiskAccess::Unknown
        );
        assert_eq!(
            probe_access([(missing.clone(), "Missing")]).status,
            FullDiskAccess::Unknown
        );
        let granted = probe_access([(missing.clone(), "Missing"), (readable.clone(), "Readable")]);
        assert_eq!(granted.status, FullDiskAccess::Granted);
        assert_eq!(granted.readable, ["Readable"]);
        assert!(granted.denied.is_empty());
        std::fs::remove_dir_all(readable).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn probe_reports_denied_for_unreadable_folder() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("denied");
        let locked = dir.join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = probe_access([(locked.clone(), "Locked")]);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        // Running as root can read anything; otherwise this must be Denied.
        assert!(matches!(
            result.status,
            FullDiskAccess::Denied | FullDiskAccess::Granted
        ));
        if result.status == FullDiskAccess::Denied {
            assert_eq!(result.denied, ["Locked"]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn macos_refusals_are_permission_denied() {
        // TCC answers EPERM (1) or EACCES (13); both must read as "denied", not "missing".
        for errno in [1, 13] {
            assert_eq!(
                std::io::Error::from_raw_os_error(errno).kind(),
                ErrorKind::PermissionDenied
            );
        }
        assert_eq!(
            std::io::Error::from_raw_os_error(2).kind(),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn adds_dedupes_and_merges_excluded_folders() {
        let root = temp("excl");
        let a = root.join("a");
        let a_sub = a.join("sub");
        std::fs::create_dir_all(&a_sub).unwrap();
        let file = root.join("file.txt");
        std::fs::write(&file, b"x").unwrap();

        let mut list = Vec::new();
        assert!(matches!(
            add_excluded(&mut list, &a_sub),
            AddExcluded::Added(_)
        ));
        assert!(matches!(
            add_excluded(&mut list, &a_sub),
            AddExcluded::AlreadyCovered(_)
        ));
        // Parent replaces the child entry.
        assert!(matches!(add_excluded(&mut list, &a), AddExcluded::Added(_)));
        assert_eq!(list, vec![paths::standardize(&a)]);
        // Child of an excluded folder is already covered.
        assert!(matches!(
            add_excluded(&mut list, &a_sub),
            AddExcluded::AlreadyCovered(_)
        ));
        assert!(matches!(
            add_excluded(&mut list, &file),
            AddExcluded::Rejected(_)
        ));
        assert!(matches!(
            add_excluded(&mut list, Path::new("/")),
            AddExcluded::Rejected(_)
        ));

        assert!(remove_excluded(&mut list, &paths::standardize(&a)));
        assert!(!remove_excluded(&mut list, &a));
        assert!(list.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn display_helpers() {
        let home = paths::home();
        #[cfg(target_os = "macos")]
        {
            assert_eq!(display_path(&home), "~");
            assert_eq!(display_path(&home.join("Code/app")), "~/Code/app");
        }
        #[cfg(windows)]
        assert_eq!(display_path(&home), home.display().to_string());
        assert_eq!(display_path(Path::new("/opt/x")), "/opt/x");
        assert_eq!(folder_name(Path::new("/a/b/c")), "c");
        assert_eq!(folder_name(Path::new("/")), "/");
    }

    #[test]
    fn cache_dir_is_inside_user_caches() {
        let dir = app_cache_dir();
        #[cfg(target_os = "macos")]
        assert!(dir.ends_with("Library/Caches/com.nextera.neracleaner"));
        #[cfg(windows)]
        assert!(dir.ends_with("Nera Cleaner/Cache"));
        // Read-only: size of a missing or existing folder never panics.
        let _ = app_cache_size();
    }
}
