//! Well-known locations.

use std::path::{Path, PathBuf};

/// Bundle identifier of the app; used for its own cache/support folders.
pub const BUNDLE_ID: &str = "com.nextera.neracleaner";

/// The user's home folder (`$HOME` on macOS, `%USERPROFILE%` on Windows).
pub fn home() -> PathBuf {
    crate::sys::home()
}

/// `~/<rel>`
pub fn home_join(rel: impl AsRef<Path>) -> PathBuf {
    home().join(rel)
}

/// Nera's settings and history folder: `~/Library/Application Support/Nera
/// Cleaner` on macOS, `%APPDATA%\Nera Cleaner` on Windows. Reading never
/// creates it; writers create it (see [`crate::fs_util::write_atomic`]).
pub fn app_support_dir() -> PathBuf {
    crate::sys::app_support_dir()
}

/// `%APPDATA%`: roaming application data.
#[cfg(windows)]
pub fn roaming_app_data() -> PathBuf {
    crate::sys::roaming_app_data()
}

/// `%LOCALAPPDATA%`: local application data and caches.
#[cfg(windows)]
pub fn local_app_data() -> PathBuf {
    crate::sys::local_app_data()
}

/// Drive Windows is installed on, with a trailing separator (`C:\`).
#[cfg(windows)]
pub fn system_drive() -> PathBuf {
    crate::sys::system_drive()
}

/// Normalise a path for prefix comparisons: absolute, no `.`/`..`, no trailing
/// separator. macOS also folds `/private/var` / `/private/tmp` to `/var` /
/// `/tmp`; Windows drops the `\\?\` prefix and upper-cases the drive letter.
/// Does not touch the file system (symlinks are not resolved).
pub fn standardize(path: impl AsRef<Path>) -> PathBuf {
    crate::sys::standardize(path.as_ref())
}

/// True when `path` equals `prefix` or lies inside it (component-wise;
/// ignoring case on Windows).
pub fn is_under(path: impl AsRef<Path>, prefix: impl AsRef<Path>) -> bool {
    crate::sys::starts_with(&standardize(path), &standardize(prefix))
}

pub fn is_under_any<P: AsRef<Path>>(path: impl AsRef<Path>, prefixes: &[P]) -> bool {
    let path = standardize(path);
    prefixes
        .iter()
        .any(|p| crate::sys::starts_with(&path, &standardize(p)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn standardizes_paths() {
        assert_eq!(
            standardize("/private/var/folders/x/../y/"),
            PathBuf::from("/var/folders/y")
        );
        assert_eq!(standardize("/Users/a/./b"), PathBuf::from("/Users/a/b"));
    }

    #[cfg(windows)]
    #[test]
    fn standardizes_paths() {
        assert_eq!(
            standardize(r"c:\Users\a\.\x\..\b\"),
            PathBuf::from(r"C:\Users\a\b")
        );
        assert_eq!(standardize(r"\\?\C:\Users\a"), PathBuf::from(r"C:\Users\a"));
        assert_eq!(
            standardize(r"\\server\share\x\"),
            PathBuf::from(r"\\server\share\x")
        );
        assert_eq!(standardize("C:/"), PathBuf::from(r"C:\"));
    }

    #[cfg(windows)]
    #[test]
    fn prefix_ignores_case_and_keeps_drives_apart() {
        assert!(is_under(r"C:\WINDOWS\System32", r"c:\Windows"));
        assert!(!is_under(r"D:\Windows", r"C:\Windows"));
    }

    #[test]
    fn prefix_is_component_wise() {
        assert!(is_under("/a/b/c", "/a/b"));
        assert!(is_under("/a/b", "/a/b"));
        assert!(!is_under("/a/bc", "/a/b"));
        assert!(is_under_any("/a/b/c", &["/x", "/a"]));
    }

    #[test]
    fn home_is_absolute() {
        assert!(home().is_absolute());
        assert!(app_support_dir().ends_with("Nera Cleaner"));
    }
}
