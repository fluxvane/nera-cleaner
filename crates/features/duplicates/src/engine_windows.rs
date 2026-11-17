//! Windows half of the duplicate finder engine: default folders, the walk and
//! file identity.
//!
//! - Default folders are the user's known folders (Downloads, Documents,
//!   Desktop, Pictures), wherever they live (OneDrive backup moves them).
//! - The walk never enters junctions or directory symlinks (std reports them
//!   as `is_symlink()`, not `is_dir()`) and never compares symbolic links. It
//!   skips the Recycle Bin, `System Volume Information` and the folders
//!   Windows and apps own (`AppData`, `Windows`, `Program Files`,
//!   `ProgramData`) unless the chosen folder lies inside one. Hidden means a
//!   dot-name or the HIDDEN attribute.
//! - Online-only OneDrive (cloud files) placeholders are skipped: reading
//!   them would download them.
//! - Hard links are found with `nera_core::fs_util::file_key` (volume serial,
//!   file index, link count). It opens the file, so it only runs for files
//!   that share their size with another one, never for every file walked.
//! - Clones: APFS clones do not exist on NTFS, so there is no clone id or
//!   private size to read. ReFS block cloning (Dev Drive) can make two files
//!   share extents, but Windows exposes no per-file way to tell; detecting it
//!   is out of scope, so such files count as independent copies.

use super::{
    display_name, is_hidden, normalize_roots, Candidate, Phase, ScanOptions, ScanProgress,
    ScanReport,
};
use nera_core::fs_util::Cancel;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::windows::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x4_0000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;

/// Folders skipped wherever they appear: items in the Recycle Bin are not
/// duplicates worth cleaning, and restore points are not user files.
const SKIPPED_NAMES: [&str; 2] = ["$Recycle.Bin", "System Volume Information"];

/// True when the file's data is not on this PC (OneDrive "online-only" and
/// other cloud-files placeholders, HSM offline files): reading it would
/// download it.
pub fn is_cloud_only(attributes: u32) -> bool {
    attributes
        & (FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
            | FILE_ATTRIBUTE_RECALL_ON_OPEN
            | FILE_ATTRIBUTE_OFFLINE)
        != 0
}

/// The user's Downloads, Documents, Desktop and Pictures folders that exist.
pub fn default_folders() -> Vec<PathBuf> {
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Pictures,
    };
    let mut out: Vec<PathBuf> = Vec::new();
    for (id, fallback) in [
        (FOLDERID_Downloads, "Downloads"),
        (FOLDERID_Documents, "Documents"),
        (FOLDERID_Desktop, "Desktop"),
        (FOLDERID_Pictures, "Pictures"),
    ] {
        let path = known_folder(&id).unwrap_or_else(|| nera_core::paths::home_join(fallback));
        let path = nera_core::paths::standardize(path);
        if path.is_dir() && !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

/// Current location of a known folder (`SHGetKnownFolderPath`).
fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DONT_VERIFY};

    let mut raw: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: valid GUID pointer and out-pointer; a null token means the
    // current user.
    let hr = unsafe {
        SHGetKnownFolderPath(
            id,
            KF_FLAG_DONT_VERIFY as u32,
            std::ptr::null_mut(),
            &mut raw,
        )
    };
    let path = (hr >= 0 && !raw.is_null()).then(|| {
        // SAFETY: on success `raw` is a NUL-terminated wide string.
        let len = (0..).take_while(|&i| unsafe { *raw.add(i) } != 0).count();
        // SAFETY: `len` u16s before the terminator are readable.
        let wide = unsafe { std::slice::from_raw_parts(raw, len) };
        PathBuf::from(std::ffi::OsString::from_wide(wide))
    });
    // SAFETY: the buffer must be freed whether or not the call succeeded;
    // freeing null is a no-op.
    unsafe { CoTaskMemFree(raw as *const std::ffi::c_void) };
    path.filter(|p| p.is_absolute())
}

/// Folders Windows and apps own, like `~/Library` on macOS: only scanned when
/// the chosen folder lies inside one. Standardized.
pub fn protected_dirs() -> Vec<PathBuf> {
    let drive = nera_core::paths::system_drive();
    let mut dirs = vec![nera_core::paths::home_join("AppData")];
    for var in [
        "SystemRoot",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramData",
    ] {
        if let Some(p) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            dirs.push(PathBuf::from(p));
        }
    }
    for name in [
        "Windows",
        "Program Files",
        "Program Files (x86)",
        "ProgramData",
    ] {
        dirs.push(drive.join(name));
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for d in dirs.iter().map(nera_core::paths::standardize) {
        if !out.iter().any(|o| same_path(o, &d)) {
            out.push(d);
        }
    }
    out
}

/// Case-insensitive equality of two standardized paths.
fn same_path(a: &Path, b: &Path) -> bool {
    a.as_os_str().eq_ignore_ascii_case(b.as_os_str())
}

/// True when the directory at `path` (named `name`) is not searched. `skip`
/// holds the protected folders that do not contain the chosen folder.
fn skip_dir(path: &Path, name: &OsStr, skip: &[PathBuf]) -> bool {
    SKIPPED_NAMES.iter().any(|n| name.eq_ignore_ascii_case(n))
        || skip.iter().any(|d| same_path(path, d))
}

/// Hidden the way File Explorer and developers see it: the HIDDEN attribute,
/// or a dot-name.
fn hidden(entry: &walkdir::DirEntry) -> bool {
    is_hidden(entry.file_name())
        || entry
            .metadata()
            .is_ok_and(|m| m.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0)
}

/// Fill in the volume, file index and hard-link count. Opens the file, so it
/// is only called for files that share their size with another file. An error
/// leaves the file out of the comparison (counted as unreadable), so a copy is
/// never assumed independent without checking.
pub(super) fn identify(candidate: &mut Candidate) -> std::io::Result<()> {
    let key = nera_core::fs_util::file_key(&candidate.path, &candidate.meta)
        .ok_or_else(|| std::io::Error::other("file identity unavailable"))?;
    candidate.dev = key.volume;
    candidate.ino = key.index;
    candidate.nlink = key.links.max(1);
    Ok(())
}

/// No APFS clones on Windows (see the module docs): always `None`.
pub fn clone_id(_path: &Path) -> Option<u64> {
    None
}

/// No per-file private size or clone id on Windows (see the module docs):
/// always `None`, so files only share data through hard links.
pub fn shared_info(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// Phase 1: walk every root and bucket eligible files by size. `None` when
/// cancelled.
pub(super) fn index(
    opts: &ScanOptions,
    cancel: &Cancel,
    progress: &(dyn Fn(ScanProgress) + Sync),
    report: &mut ScanReport,
) -> Option<HashMap<u64, Vec<Candidate>>> {
    let min_size = opts.min_size.max(1);
    let excluded: Vec<PathBuf> = opts
        .excluded
        .iter()
        .map(nera_core::paths::standardize)
        .collect();
    let protected = protected_dirs();
    let mut by_size: HashMap<u64, Vec<Candidate>> = HashMap::new();
    let mut seen = 0usize;

    for root in normalize_roots(&opts.roots) {
        match std::fs::symlink_metadata(&root) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                report.denied.push(root);
                continue;
            }
            Err(_) => {
                report.missing.push(root);
                continue;
            }
        }
        report.folders_searched += 1;
        // AppData, Windows, Program Files...: only scanned when chosen explicitly.
        let skip: Vec<PathBuf> = protected
            .iter()
            .filter(|d| !nera_core::paths::is_under(&root, d))
            .cloned()
            .collect();
        let root_name = display_name(&root);
        let walker = walkdir::WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                if e.depth() == 0 {
                    return true;
                }
                if !opts.include_hidden && hidden(e) {
                    return false;
                }
                // Junctions and directory symlinks are `is_symlink()`, so they
                // never get here as directories and are never entered.
                if e.file_type().is_dir() && skip_dir(e.path(), e.file_name(), &skip) {
                    return false;
                }
                excluded.is_empty() || !nera_core::paths::is_under_any(e.path(), &excluded)
            });

        for entry in walker {
            seen += 1;
            if seen.is_multiple_of(512) {
                if cancel.is_cancelled() {
                    return None;
                }
                progress(ScanProgress {
                    phase: Phase::Indexing,
                    files_seen: seen,
                    done: 0,
                    total: 0,
                    current: Some(root_name.clone()),
                });
            }
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    let denied = err
                        .io_error()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied);
                    if let (true, Some(p)) = (denied, err.path()) {
                        report.denied.push(p.to_path_buf());
                    }
                    continue;
                }
            };
            // Symbolic links are never followed nor compared.
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let size = meta.len();
            if size < min_size || is_cloud_only(meta.file_attributes()) {
                continue;
            }
            report.files_considered += 1;
            // Explorer's "Size on disk" for sparse and compressed files, the
            // length otherwise.
            let allocated = nera_core::fs_util::allocated_size_at(entry.path(), &meta);
            by_size.entry(size).or_default().push(Candidate {
                path: entry.into_path(),
                size,
                modified: meta.modified().ok(),
                // Placeholder identity, unique per file (no volume serial is
                // `u64::MAX`): `identify` replaces it before any comparison.
                dev: u64::MAX,
                ino: seen as u64,
                nlink: 1,
                allocated,
                meta,
            });
        }
    }
    if cancel.is_cancelled() {
        return None;
    }
    Some(by_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_only_attributes() {
        assert!(!is_cloud_only(0x20)); // ARCHIVE
        assert!(!is_cloud_only(0x20 | 0x400 | 0x8_0000)); // reparse point, pinned: hydrated
        assert!(is_cloud_only(0x40_0000)); // RECALL_ON_DATA_ACCESS (OneDrive online-only)
        assert!(is_cloud_only(0x4_0000)); // RECALL_ON_OPEN
        assert!(is_cloud_only(0x1000)); // OFFLINE
    }

    #[test]
    fn skip_rules_are_case_insensitive_and_exact() {
        let appdata = nera_core::paths::standardize(nera_core::paths::home_join("AppData"));
        let skip = vec![appdata.clone()];
        let lower = PathBuf::from(appdata.to_string_lossy().to_lowercase());
        assert!(skip_dir(&appdata, OsStr::new("AppData"), &skip));
        assert!(skip_dir(&lower, OsStr::new("appdata"), &skip));
        assert!(!skip_dir(
            &appdata.join("Local"),
            OsStr::new("Local"),
            &skip
        ));
        assert!(skip_dir(
            Path::new(r"D:\$RECYCLE.BIN"),
            OsStr::new("$RECYCLE.BIN"),
            &[]
        ));
        assert!(skip_dir(
            Path::new(r"D:\System Volume Information"),
            OsStr::new("System Volume Information"),
            &[]
        ));
        assert!(!skip_dir(
            Path::new(r"D:\Photos"),
            OsStr::new("Photos"),
            &skip
        ));
    }

    #[test]
    fn protected_dirs_cover_appdata_windows_and_program_files() {
        let dirs = protected_dirs();
        let has = |p: PathBuf| {
            let p = nera_core::paths::standardize(p);
            dirs.iter().any(|d| same_path(d, &p))
        };
        assert!(has(nera_core::paths::home_join("AppData")));
        let drive = nera_core::paths::system_drive();
        assert!(has(drive.join("Windows")));
        assert!(has(drive.join("Program Files")));
        assert!(has(drive.join("Program Files (x86)")));
        // No duplicates once case is ignored.
        for (i, a) in dirs.iter().enumerate() {
            assert!(!dirs[i + 1..].iter().any(|b| same_path(a, b)), "{a:?}");
        }
    }

    #[test]
    fn default_folders_are_existing_known_folders() {
        let folders = default_folders();
        assert!(folders.iter().all(|f| f.is_dir() && f.is_absolute()));
        // Documents always resolves, even when OneDrive moved it.
        let docs = known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Documents).unwrap();
        assert!(docs.is_absolute(), "{docs:?}");
    }
}
