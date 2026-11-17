//! File-system primitives shared by all features: sizing, safe removal,
//! process lookups. Removal here is the ONLY sanctioned way to delete.

use crate::safety;
pub use crate::sys::FileKey;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Cooperative cancellation flag passed into long scans.
#[derive(Clone, Default, Debug)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Debug)]
pub struct SizeOptions {
    pub include_hidden: bool,
    pub max_depth: usize,
    /// Subtrees to skip entirely (already standardized or not).
    pub excluding: Vec<PathBuf>,
}

impl Default for SizeOptions {
    fn default() -> Self {
        Self {
            include_hidden: true,
            max_depth: 64,
            excluding: Vec::new(),
        }
    }
}

/// On-disk size of one item from its metadata alone, not following symlinks:
/// `st_blocks` on macOS, the logical length on Windows (whose metadata has no
/// allocation figure). Prefer [`allocated_size_at`] when the path is at hand.
pub fn allocated_size(meta: &std::fs::Metadata) -> u64 {
    crate::sys::allocated_size(meta)
}

/// Allocated size as Finder and Explorer report it. Differs from
/// [`allocated_size`] for transparently compressed files on macOS and for
/// sparse, compressed and cloud-only (OneDrive) files on Windows.
pub fn allocated_size_at(path: &Path, meta: &std::fs::Metadata) -> u64 {
    crate::sys::allocated_size_at(path, meta)
}

/// Identity and hard-link count of a file. Free on macOS (read from `meta`);
/// on Windows it opens the file, so call it only when the answer matters.
pub fn file_key(path: &Path, meta: &std::fs::Metadata) -> Option<FileKey> {
    crate::sys::file_key(path, meta)
}

/// Recursive allocated size of `path` (file or directory), counted the way
/// `du -skx` and Finder do: symlinks and junctions are not followed, a
/// hard-linked file is counted once per call, other volumes mounted inside
/// are skipped, and compressed files use their real allocation. Unreadable entries are
/// skipped. Returns the partial total when cancelled.
pub fn dir_size(path: &Path, opts: &SizeOptions, cancel: &Cancel) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return allocated_size_at(path, &meta);
    }
    let excluding: Vec<PathBuf> = opts
        .excluding
        .iter()
        .map(crate::paths::standardize)
        .collect();
    let volumes = crate::sys::walk_volumes(path);
    let mut seen_links: std::collections::HashSet<(u64, u64)> = std::collections::HashSet::new();
    let mut total = 0u64;
    let mut stack = vec![(path.to_path_buf(), 0usize)];
    let mut visited = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited.is_multiple_of(512) && cancel.is_cancelled() {
                return total;
            }
            let name = entry.file_name();
            if !opts.include_hidden && name.to_string_lossy().starts_with('.') {
                continue;
            }
            let child = entry.path();
            if !excluding.is_empty() && crate::paths::is_under_any(&child, &excluding) {
                continue;
            }
            // DirEntry::metadata does not follow symlinks.
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                if depth < opts.max_depth && crate::sys::may_enter(&meta, &volumes) {
                    stack.push((child, depth + 1));
                }
            } else {
                if file_key(&child, &meta)
                    .is_some_and(|key| key.links > 1 && !seen_links.insert(key.id()))
                {
                    continue;
                }
                total += allocated_size_at(&child, &meta);
            }
        }
    }
    total
}

/// Write `bytes` to `path` atomically (temp file + rename), creating the
/// parent folder first.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Permanently delete `path` (file or directory) if safety allows.
pub fn remove_item(path: &Path) -> std::io::Result<()> {
    if !safety::is_deletable(path) {
        return Err(refused(path));
    }
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Move `path` to the user's Trash (the Recycle Bin on Windows) if safety
/// allows. Recoverable.
pub fn trash_item(path: &Path) -> std::io::Result<()> {
    if !safety::is_trashable(path) {
        return Err(refused(path));
    }
    crate::sys::trash(path)
}

/// Remove per the user's preference: Trash when `to_trash`, else delete.
pub fn remove_or_trash(path: &Path, to_trash: bool) -> std::io::Result<()> {
    if to_trash {
        trash_item(path)
    } else {
        remove_item(path)
    }
}

fn refused(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("Protected location, not removed: {}", path.display()),
    )
}

/// Executable path of a process, if readable.
pub fn executable_path(pid: i32) -> Option<PathBuf> {
    crate::sys::executable_path(pid)
}

/// PIDs of all processes visible to this user.
pub fn all_pids() -> Vec<i32> {
    crate::sys::all_pids()
}

/// True when a process with this PID exists (even one we may not inspect).
pub fn process_alive(pid: i32) -> bool {
    crate::sys::process_alive(pid)
}

/// True when any running process's executable lives under `prefix`.
pub fn any_process_under(prefix: &Path) -> bool {
    all_pids()
        .into_iter()
        .any(|pid| executable_path(pid).is_some_and(|exe| crate::paths::is_under(exe, prefix)))
}

/// System Settings pane for Full Disk Access (macOS).
#[cfg(target_os = "macos")]
pub const FULL_DISK_ACCESS_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";

/// Nera's own cache folder: `~/Library/Caches/<bundle id>` on macOS,
/// `%LOCALAPPDATA%\Nera Cleaner\Cache` on Windows.
pub fn app_cache_dir() -> PathBuf {
    crate::sys::app_cache_dir()
}

/// Delete everything inside Nera's own cache folder (the folder itself is
/// kept). This is the one sanctioned bypass of `safety::is_live_app_state`,
/// which otherwise protects the folder: it touches nothing outside it and
/// never follows symlinks out of it. Returns the bytes freed.
pub fn clear_app_cache() -> std::io::Result<u64> {
    let dir = app_cache_dir();
    let mut freed = 0;
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !crate::paths::is_under(&path, &dir) || path == dir {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let size = dir_size(&path, &SizeOptions::default(), &Cancel::new());
        let result = if meta.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if result.is_ok() {
            freed += size;
        }
    }
    Ok(freed)
}

/// Reveal a file in Finder (File Explorer on Windows).
pub fn reveal_in_finder(path: &Path) {
    crate::sys::reveal(path);
}

/// Open a file, folder or URL with its default handler.
pub fn open(target: impl AsRef<std::ffi::OsStr>) {
    crate::sys::open(target.as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_a_directory_and_respects_exclusions() {
        let dir = std::env::temp_dir().join(format!("nera-size-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("keep")).unwrap();
        std::fs::create_dir_all(dir.join("skip")).unwrap();
        std::fs::write(dir.join("keep/a"), vec![1u8; 100_000]).unwrap();
        std::fs::write(dir.join("skip/b"), vec![1u8; 100_000]).unwrap();
        let all = dir_size(&dir, &SizeOptions::default(), &Cancel::new());
        let opts = SizeOptions {
            excluding: vec![dir.join("skip")],
            ..Default::default()
        };
        let partial = dir_size(&dir, &opts, &Cancel::new());
        assert!(all >= 200_000 && partial >= 100_000 && partial < all);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn counts_a_hard_linked_file_once() {
        let dir = std::env::temp_dir().join(format!("nera-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a"), vec![1u8; 200_000]).unwrap();
        let one = dir_size(&dir, &SizeOptions::default(), &Cancel::new());
        std::fs::hard_link(dir.join("a"), dir.join("b")).unwrap();
        assert_eq!(dir_size(&dir, &SizeOptions::default(), &Cancel::new()), one);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn writes_atomically_creating_the_folder() {
        let dir = std::env::temp_dir().join(format!("nera-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_atomic(&dir.join("sub/x.json"), b"{}").unwrap();
        assert_eq!(std::fs::read(dir.join("sub/x.json")).unwrap(), b"{}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn finds_own_process() {
        let exe = std::env::current_exe().unwrap();
        assert!(any_process_under(exe.parent().unwrap()));
        assert_eq!(executable_path(std::process::id() as i32).unwrap(), exe);
        assert!(process_alive(std::process::id() as i32));
        assert!(all_pids().contains(&(std::process::id() as i32)));
    }

    #[test]
    fn refuses_protected_removal() {
        #[cfg(target_os = "macos")]
        assert!(remove_item(Path::new("/System")).is_err());
        #[cfg(windows)]
        assert!(remove_item(&crate::paths::system_drive().join("Windows")).is_err());
        assert!(trash_item(&crate::paths::home()).is_err());
    }
}
