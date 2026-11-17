//! macOS half of the duplicate finder engine, moved unchanged from
//! `engine.rs`: default folders, the walk (packages, `~/Library`, `.Trash`,
//! iCloud placeholders skipped; inode, link count and blocks read from the
//! walk's metadata) and APFS clone detection.

use super::{
    display_name, is_hidden, is_package, normalize_roots, Candidate, Phase, ScanOptions,
    ScanProgress, ScanReport,
};
use nera_core::fs_util::Cancel;
use std::collections::HashMap;
use std::os::macos::fs::MetadataExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

/// macOS `SF_DATALESS`: an iCloud placeholder whose content is not on disk.
/// Reading it would trigger a download, so it is skipped.
const SF_DATALESS: u32 = 0x4000_0000;

/// The default folders that exist on this Mac.
pub fn default_folders() -> Vec<PathBuf> {
    ["Downloads", "Documents", "Desktop", "Pictures"]
        .iter()
        .map(nera_core::paths::home_join)
        .filter(|p| p.is_dir())
        .collect()
}

/// Nothing to do: device, inode and link count came with the walk's metadata.
pub(super) fn identify(_candidate: &mut Candidate) -> std::io::Result<()> {
    Ok(())
}

/// APFS clone identifier: files cloned from each other report the same id,
/// until one of them is modified. `None` on volumes without clones or on error.
pub fn clone_id(path: &Path) -> Option<u64> {
    shared_info(path).map(|(_, id)| id).filter(|id| *id != 0)
}

/// `(private size, clone id)` from APFS (`ATTR_CMNEXT_PRIVATESIZE` /
/// `ATTR_CMNEXT_CLONEID`): the bytes only this file holds, and an identifier
/// that clones of one another share. `None` when the volume cannot say.
pub fn shared_info(path: &Path) -> Option<(u64, u64)> {
    use std::ffi::{c_char, c_int, c_void, CString};
    use std::os::unix::ffi::OsStrExt as _;

    #[repr(C)]
    struct AttrList {
        bitmapcount: u16,
        reserved: u16,
        commonattr: u32,
        volattr: u32,
        dirattr: u32,
        fileattr: u32,
        forkattr: u32,
    }
    // Attributes come back in bit order: private size (0x8), then clone id (0x100).
    #[repr(C, packed(4))]
    struct Reply {
        length: u32,
        private_size: i64,
        clone_id: u64,
    }
    extern "C" {
        fn getattrlist(
            path: *const c_char,
            list: *mut c_void,
            buf: *mut c_void,
            size: usize,
            options: u32,
        ) -> c_int;
    }
    const ATTR_BIT_MAP_COUNT: u16 = 5;
    const ATTR_CMNEXT_PRIVATESIZE: u32 = 0x0000_0008;
    const ATTR_CMNEXT_CLONEID: u32 = 0x0000_0100;
    const FSOPT_NOFOLLOW: u32 = 0x0000_0001;
    const FSOPT_ATTR_CMN_EXTENDED: u32 = 0x0000_0020;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut list = AttrList {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: ATTR_CMNEXT_PRIVATESIZE | ATTR_CMNEXT_CLONEID,
    };
    let mut reply = Reply {
        length: 0,
        private_size: -1,
        clone_id: 0,
    };
    // SAFETY: `list` and `reply` are valid for the sizes passed; the kernel
    // writes at most `size_of::<Reply>()` bytes and reads `c_path` as a C string.
    let rc = unsafe {
        getattrlist(
            c_path.as_ptr(),
            (&mut list as *mut AttrList).cast(),
            (&mut reply as *mut Reply).cast(),
            std::mem::size_of::<Reply>(),
            FSOPT_NOFOLLOW | FSOPT_ATTR_CMN_EXTENDED,
        )
    };
    let (private, id) = (reply.private_size, reply.clone_id);
    (rc == 0 && reply.length as usize >= std::mem::size_of::<Reply>() && private >= 0)
        .then_some((private as u64, id))
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
    let home_library = nera_core::paths::standardize(nera_core::paths::home_join("Library"));
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
        // ~/Library holds live app data; only scan it when chosen explicitly.
        let skip_library = !root.starts_with(&home_library);
        let root_name = display_name(&root);
        let walker = walkdir::WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                if e.depth() == 0 {
                    return true;
                }
                if !opts.include_hidden && is_hidden(e.file_name()) {
                    return false;
                }
                // Items already in a Trash are not duplicates worth cleaning.
                if e.file_type().is_dir() && e.file_name() == ".Trash" {
                    return false;
                }
                if e.file_type().is_dir() {
                    let p = e.path();
                    if is_package(p) || (skip_library && p == home_library) {
                        return false;
                    }
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
            // Symlinks are never followed nor compared.
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let size = meta.len();
            if size < min_size || meta.st_flags() & SF_DATALESS != 0 {
                continue;
            }
            report.files_considered += 1;
            by_size.entry(size).or_default().push(Candidate {
                path: entry.into_path(),
                size,
                modified: meta.modified().ok(),
                dev: meta.dev(),
                ino: meta.ino(),
                nlink: meta.nlink(),
                allocated: meta.blocks() * 512,
            });
        }
    }
    if cancel.is_cancelled() {
        return None;
    }
    Some(by_size)
}
