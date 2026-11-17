//! Windows side of the Space Lens engine. Exports the same names as
//! `macos.rs`.
//!
//! - Sizes: `nera_core::fs_util::allocated_size_at`, i.e. the length Explorer
//!   shows as "Size" for ordinary files and the real allocation for sparse,
//!   NTFS-compressed and cloud-only (OneDrive) files.
//! - Volume boundaries: junctions, directory symlinks and volume mount points
//!   are reparse points that `std` reports as links (`is_dir()` is false), so a
//!   walk never enters them. Every directory a walk sees is on its volume.
//! - Hard links: Windows metadata has no link count, and asking for one opens
//!   the file. They are only common inside the Windows folder (WinSxS ↔
//!   System32), so they are counted once there and once per name elsewhere,
//!   which is also how Explorer adds up a folder.

use super::ItemKind;
use nera_core::fs_util::file_key;
use nera_core::paths::{home, is_under, standardize, system_drive};
use nera_core::safety;
use std::collections::HashMap;
use std::fs::{DirEntry, Metadata};
use std::os::windows::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::Storage::FileSystem::{
    GetDriveTypeW, GetVolumeInformationW, GetVolumePathNameW,
};

/// Nothing needs skipping for volume reasons: other volumes are only reachable
/// through mount points, which are never entered.
pub const SKIP_SUBTREES: [&str; 0] = [];

/// What Windows calls a junction or symbolic link in plain words.
pub const LINK_LABEL: &str = "Link";

/// `GetDriveTypeW` results (`Win32::System::WindowsProgramming`).
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;

/// Windows has no bundles: every directory is a folder.
pub fn dir_kind(_path: &Path, _ext: &str) -> ItemKind {
    ItemKind::Folder
}

/// A volume mounted into a folder: a mount-point reparse point whose target is
/// a volume (`\\?\Volume{…}\`) rather than a folder (a junction).
pub fn is_volume_mount(path: &Path, meta: &Metadata) -> bool {
    meta.file_type().is_symlink()
        && std::fs::read_link(path).is_ok_and(|t| {
            t.to_string_lossy()
                .to_ascii_lowercase()
                .starts_with(r"\\?\volume{")
        })
}

/// Every directory a walk can reach is on the volume it started on (see the
/// module docs), so all share one id.
pub fn volume_id(_meta: &Metadata) -> u64 {
    0
}

/// See [`volume_id`].
pub fn volume_set(_root: &Path) -> Vec<u64> {
    vec![0]
}

/// Drives are not listed under one folder as `/Volumes` lists them on macOS.
pub fn lists_volumes(_dir: &Path) -> bool {
    false
}

/// `%SystemRoot%` (`C:\Windows`).
fn system_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| system_drive().join("Windows"))
    })
}

/// Whether files directly inside `dir` are checked for hard links (see the
/// module docs for why only the Windows folder).
pub fn tracks_links(dir: &Path) -> bool {
    tracks_links_under(dir, system_root())
}

fn tracks_links_under(dir: &Path, root: &Path) -> bool {
    is_under(dir, root)
}

/// Volume serial and file index of a file with more than one name. Opens the
/// file (one `CreateFileW` + `GetFileInformationByHandle`).
pub fn link_key(entry: &DirEntry, meta: &Metadata) -> Option<(u64, u64)> {
    file_key(&entry.path(), meta)
        .filter(|k| k.links > 1)
        .map(|k| k.id())
}

/// The Recycle Bin of the drive holding `root`: its items are already there.
pub fn trash_dirs(root: &Path) -> Vec<PathBuf> {
    root.ancestors()
        .last()
        .map(|drive| vec![drive.join("$Recycle.Bin")])
        .unwrap_or_default()
}

/// Space a file takes, as `nera_core::fs_util::allocated_size_at` reports it.
/// Directories and links hold no data of their own.
pub fn allocated(path: impl FnOnce() -> PathBuf, meta: &Metadata) -> u64 {
    if !meta.is_file() {
        return 0;
    }
    nera_core::fs_util::allocated_size_at(&path(), meta)
}

// MARK: - Safety

/// Lower-cased path components below the drive or share.
fn names(p: &Path) -> Vec<String> {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect()
}

fn same(a: &Path, b: &Path) -> bool {
    is_under(a, b) && is_under(b, a)
}

/// Why `path` must not be moved to the Recycle Bin, or `None` when it may be.
/// Wraps `nera_core::safety` with a reason a person can act on.
pub fn protection_reason(path: &Path) -> Option<&'static str> {
    let p = standardize(path);
    if same(&p, &home()) {
        return Some("This is your user folder.");
    }
    let n = names(&p);
    if n.first().is_some_and(|top| top == "$recycle.bin") {
        return Some("This item is already in the Recycle Bin.");
    }
    if !safety::is_trashable(path) {
        if safety::is_live_app_state(path) {
            return Some("A running app uses this item; removing it could break the app.");
        }
        return Some(system_reason(&p, &n));
    }
    if !safety::is_deletable(path) {
        return Some("This is an essential folder that Windows and your apps rely on.");
    }
    None
}

/// Reason for a location `nera_core::safety` refuses to trash.
fn system_reason(p: &Path, n: &[String]) -> &'static str {
    let ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    let lower = p.to_string_lossy().to_ascii_lowercase();
    if matches!(ext.as_deref(), Some("vhdx" | "vhd" | "avhdx"))
        || lower.contains(r"\docker")
        || lower.contains(r"\wsl")
    {
        return "A virtual disk (Docker, WSL or Hyper-V). Free this space from that app instead.";
    }
    match n {
        [] => "This is the root of a drive.",
        [file] if file == "pagefile.sys" || file == "swapfile.sys" => {
            "Windows uses this file as virtual memory."
        }
        [file] if file == "hiberfil.sys" => {
            "Windows uses this file for hibernation and Fast startup."
        }
        [top, ..] if top == "program files" || top == "program files (x86)" => {
            "Installed apps live here. Uninstall them in Settings instead."
        }
        [top, ..] if top == "system volume information" => {
            "Windows keeps restore points and drive data here."
        }
        _ => "Windows needs this location to work.",
    }
}

// MARK: - Disk capacity

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Root of the volume holding `path` (`C:\`, or a folder a volume is mounted on).
fn volume_root(path: &Path) -> Option<PathBuf> {
    let name = wide(path);
    let mut buf = [0u16; 1024];
    // SAFETY: NUL-terminated input and an output buffer of the stated length.
    let ok = unsafe { GetVolumePathNameW(name.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    (ok != 0).then(|| PathBuf::from(from_wide(&buf)))
}

/// True when `path` lives on the system drive (the volume the disk tiles
/// describe), so a share "of used space" is meaningful.
pub fn on_startup_disk(path: &Path) -> bool {
    if std::fs::metadata(path).is_err() {
        return false;
    }
    volume_root(path).is_some_and(|root| standardize(root) == standardize(system_drive()))
}

// MARK: - Permissions

/// Windows has no Full Disk Access: folders it denies are reported as unreadable.
pub fn has_full_disk_access() -> Option<bool> {
    None
}

// MARK: - Display helpers

/// Path for display, as Explorer's address bar writes it.
pub fn display_path(path: &Path) -> String {
    standardize(path).display().to_string()
}

/// Short name of a folder: the user name for the profile (as Explorer's
/// address bar shows it), the drive as Explorer names it ("Local Disk (C:)"),
/// else the last component.
pub fn folder_name(path: &Path) -> String {
    let h = home();
    if same(path, &h) {
        if let Some(n) = h.file_name() {
            return n.to_string_lossy().into_owned();
        }
    }
    match path.file_name() {
        Some(n) => n.to_string_lossy().into_owned(),
        None => drive_name(path),
    }
}

/// Explorer's name for a drive root: "<label> (C:)", "Local Disk (C:)" for an
/// unlabelled fixed disk. The path itself when the label can't be read or the
/// drive is a network share (whose query could block). Cached per root: the
/// screen asks while rendering.
fn drive_name(root: &Path) -> String {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();
    let root = standardize(root);
    let cache = CACHE.get_or_init(Default::default);
    if let Some(name) = cache.lock().unwrap().get(&root) {
        return name.clone();
    }
    let name = read_drive_name(&root).unwrap_or_else(|| root.display().to_string());
    cache.lock().unwrap().insert(root, name.clone());
    name
}

fn read_drive_name(root: &Path) -> Option<String> {
    let Some(Component::Prefix(prefix)) = root.components().next() else {
        return None;
    };
    let letter = match prefix.kind() {
        Prefix::Disk(d) | Prefix::VerbatimDisk(d) => (d as char).to_ascii_uppercase(),
        _ => return None,
    };
    let name = wide(root);
    // SAFETY: NUL-terminated root path.
    let kind = unsafe { GetDriveTypeW(name.as_ptr()) };
    if kind == DRIVE_REMOTE {
        return None;
    }
    let mut label = [0u16; 261];
    // SAFETY: NUL-terminated root, a label buffer of the stated length; the
    // other outputs are optional and passed as null.
    let ok = unsafe {
        GetVolumeInformationW(
            name.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        return None;
    }
    explorer_drive_name(&from_wide(&label), letter, kind == DRIVE_FIXED)
}

/// "<label> (C:)", or "Local Disk (C:)" for an unlabelled fixed disk; `None`
/// for other unlabelled drives, whose Explorer names this code can't confirm.
fn explorer_drive_name(label: &str, letter: char, fixed: bool) -> Option<String> {
    let label = label.trim();
    if !label.is_empty() {
        Some(format!("{label} ({letter}:)"))
    } else if fixed {
        Some(format!("Local Disk ({letter}:)"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_drives_like_explorer() {
        assert_eq!(
            explorer_drive_name("Windows", 'C', true).as_deref(),
            Some("Windows (C:)")
        );
        assert_eq!(
            explorer_drive_name("", 'C', true).as_deref(),
            Some("Local Disk (C:)")
        );
        assert_eq!(explorer_drive_name(" ", 'E', false), None);
        // The real system drive gets a name with its letter, or its path.
        let name = folder_name(&system_drive());
        let letter = system_drive().to_string_lossy().chars().next().unwrap();
        assert!(
            name.ends_with(&format!("({letter}:)")) || name == system_drive().display().to_string(),
            "{name}"
        );
    }

    #[test]
    fn hard_links_are_tracked_inside_the_windows_folder_only() {
        let root = Path::new(r"C:\Windows");
        assert!(tracks_links_under(Path::new(r"C:\Windows\WinSxS\x"), root));
        assert!(tracks_links_under(Path::new(r"c:\windows\System32"), root));
        assert!(!tracks_links_under(Path::new(r"C:\Users\a"), root));
        assert!(!tracks_links_under(Path::new(r"C:\WindowsApps"), root));
        assert!(tracks_links(system_root()));
        assert!(!tracks_links(&home()));
    }

    #[test]
    fn protection_reasons() {
        let drive = system_drive();
        let reason = |rel: &str| protection_reason(&drive.join(rel));
        assert_eq!(
            protection_reason(&drive),
            Some("This is the root of a drive.")
        );
        assert_eq!(
            reason("pagefile.sys"),
            Some("Windows uses this file as virtual memory.")
        );
        assert_eq!(
            reason("Program Files/Git"),
            Some("Installed apps live here. Uninstall them in Settings instead.")
        );
        assert_eq!(
            reason("Windows/System32"),
            Some("Windows needs this location to work.")
        );
        assert_eq!(
            reason("$Recycle.Bin/S-1-5-21/x.zip"),
            Some("This item is already in the Recycle Bin.")
        );
        assert_eq!(
            protection_reason(&home()),
            Some("This is your user folder.")
        );
        assert_eq!(
            protection_reason(&home().join("Documents")),
            Some("This is an essential folder that Windows and your apps rely on.")
        );
        assert!(protection_reason(&home().join(r"Downloads\movie.mp4")).is_none());
        assert!(protection_reason(&home().join(r"Downloads\disk.vhdx")).is_some());
    }

    #[test]
    fn recycle_bin_of_the_scanned_drive() {
        assert_eq!(
            trash_dirs(Path::new(r"D:\Data\x")),
            vec![PathBuf::from(r"D:\$Recycle.Bin")]
        );
    }

    #[test]
    fn display_and_startup_disk() {
        assert_eq!(
            display_path(Path::new(r"\\?\c:\Users\a\.\b")),
            r"C:\Users\a\b"
        );
        assert!(on_startup_disk(&home()));
        assert!(on_startup_disk(&system_drive()));
        assert!(!on_startup_disk(Path::new(r"C:\definitely\not\here")));
        let user = home().file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(folder_name(&home()), user);
        assert_eq!(folder_name(&home().join("Videos")), "Videos");
    }
}
