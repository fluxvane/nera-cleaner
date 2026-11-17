//! macOS side of the Space Lens engine: APFS sizes, device-id volume
//! boundaries, bundle classification, Finder's capacity figure and the Full
//! Disk Access probe. Moved unchanged from `engine.rs`; `windows.rs` exports
//! the same names.

use super::ItemKind;
use nera_core::fs_util::allocated_size;
use nera_core::paths::{home, standardize};
use nera_core::safety;
use std::fs::{DirEntry, Metadata};
use std::os::macos::fs::MetadataExt as MacMetadataExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Mount points and virtual trees that are never descended into unless the
/// scan starts inside them: other volumes (`/Volumes`, `/System/Volumes`
/// would count the data volume twice), devices and automounts.
pub const SKIP_SUBTREES: [&str; 6] = [
    "/System/Volumes",
    "/Volumes",
    "/dev",
    "/net",
    "/home",
    "/.vol",
];

/// Directory extensions macOS shows as a single document/bundle.
const PACKAGE_EXTENSIONS: [&str; 22] = [
    "photoslibrary",
    "musiclibrary",
    "tvlibrary",
    "imovielibrary",
    "fcpbundle",
    "aplibrary",
    "photolibrary",
    "logicx",
    "band",
    "xcarchive",
    "sparsebundle",
    "bundle",
    "framework",
    "plugin",
    "appex",
    "kext",
    "pvm",
    "vmwarevm",
    "utm",
    "playground",
    "rtfd",
    "pages",
];

/// What Finder calls a symbolic link.
pub const LINK_LABEL: &str = "Alias";

/// A `.app` folder is an application only when it is laid out like one;
/// `~/.dotnet/shared/Microsoft.NETCore.App` is just a folder with that ending.
fn looks_like_app(path: &Path) -> bool {
    ["Contents", "Wrapper", "Info.plist"]
        .iter()
        .any(|marker| path.join(marker).exists())
}

/// Kind of a directory: application, package (bundle) or plain folder.
pub fn dir_kind(path: &Path, ext: &str) -> ItemKind {
    if ext == "app" {
        if looks_like_app(path) {
            ItemKind::App
        } else {
            ItemKind::Folder
        }
    } else if PACKAGE_EXTENSIONS.contains(&ext) {
        ItemKind::Package
    } else {
        ItemKind::Folder
    }
}

/// macOS tells mounted volumes apart by device id while listing (see
/// `list_children`), never from the entry itself.
pub fn is_volume_mount(_path: &Path, _meta: &Metadata) -> bool {
    false
}

/// Device id of the volume holding an item.
pub fn volume_id(meta: &Metadata) -> u64 {
    meta.dev()
}

/// Hard links are tracked everywhere: the link count is free in `lstat`.
pub fn tracks_links(_dir: &Path) -> bool {
    true
}

/// `(device, inode)` of a file with more than one name.
pub fn link_key(_entry: &DirEntry, meta: &Metadata) -> Option<(u64, u64)> {
    (meta.nlink() > 1).then(|| (meta.dev(), meta.ino()))
}

/// Folders whose items are already in the Trash.
pub fn trash_dirs(_root: &Path) -> Vec<PathBuf> {
    vec![home().join(".Trash")]
}

/// `UF_COMPRESSED`: the file's data is stored compressed (APFS/HFS+ transparent compression).
pub const UF_COMPRESSED: u32 = 0x20;

/// Space `meta` takes on disk, as Finder and Spotlight report it
/// (`ATTR_FILE_ALLOCSIZE`). That equals `st_blocks * 512` except for
/// transparently compressed files, whose small payload sits inside the file's
/// metadata: `st_blocks` still says one 4 KB block, the file system allocates
/// none. `path` is only used for those files.
pub fn allocated(path: impl FnOnce() -> PathBuf, meta: &Metadata) -> u64 {
    let blocks = allocated_size(meta);
    if blocks == 0 || !meta.is_file() || meta.st_flags() & UF_COMPRESSED == 0 {
        return blocks;
    }
    attr_alloc_size(&path()).unwrap_or(blocks)
}

/// `ATTR_FILE_ALLOCSIZE` of a file via `getattrlist`; `None` when unavailable.
pub fn attr_alloc_size(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: zeroed attrlist is a valid "no attributes" request; fields set below.
    let mut list: libc::attrlist = unsafe { std::mem::zeroed() };
    list.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    list.fileattr = libc::ATTR_FILE_ALLOCSIZE;
    // Reply: u32 total length, then the packed off_t.
    let mut buf = [0u8; 12];
    // SAFETY: valid C string, attrlist and a buffer of the stated size.
    let rc = unsafe {
        libc::getattrlist(
            c.as_ptr(),
            (&mut list as *mut libc::attrlist).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::FSOPT_NOFOLLOW,
        )
    };
    if rc != 0 || u32::from_ne_bytes(buf[0..4].try_into().ok()?) < 12 {
        return None;
    }
    u64::try_from(i64::from_ne_bytes(buf[4..12].try_into().ok()?)).ok()
}

/// Volumes a walk may enter: the one the walk started on and, because macOS
/// presents the startup disk as two firmlinked volumes, the system and data
/// volumes. Anything else mounted inside a folder (disk images such as the
/// Simulator runtimes, devices, network shares) holds space that lives
/// elsewhere, so it is not counted (like `du -x`).
pub fn volume_set(root: &Path) -> Vec<u64> {
    let mut devs: Vec<u64> = [root, Path::new("/"), Path::new("/System/Volumes/Data")]
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok().map(|m| m.dev()))
        .collect();
    devs.sort_unstable();
    devs.dedup();
    devs
}

/// True when listing `dir` shows the mounted disks themselves.
pub fn lists_volumes(dir: &Path) -> bool {
    standardize(dir) == Path::new("/Volumes")
}

// MARK: - Safety

/// Why `path` must not be moved to the Trash, or `None` when it may be.
/// Wraps `nera_core::safety` and adds Space Lens' own guard for essential
/// user folders (moving `~/Library` or `~/Documents` would break the account).
pub fn protection_reason(path: &Path) -> Option<&'static str> {
    let p = standardize(path);
    let home = standardize(home());
    if p == home {
        return Some("This is your home folder.");
    }
    if !safety::is_trashable(path) {
        if safety::is_live_app_state(path) {
            return Some("A running app uses this item; removing it could break the app.");
        }
        return Some("macOS needs this location to work.");
    }
    if !safety::is_deletable(path) {
        let s = p.to_string_lossy();
        if s.contains("docker") || s.contains("orbstack") {
            return Some("Managed by your container app. Free this space from that app instead.");
        }
        return Some("This is an essential folder that macOS and your apps rely on.");
    }
    if p.starts_with(home.join(".Trash")) {
        return Some("This item is already in the Trash.");
    }
    None
}

// MARK: - Disk capacity

#[allow(non_upper_case_globals)]
mod cf {
    use std::ffi::c_void;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub static kCFURLVolumeAvailableCapacityForImportantUsageKey: *const c_void;
        pub fn CFURLCopyResourcePropertyForKey(
            url: *const c_void,
            key: *const c_void,
            property_value_type_ref_ptr: *mut c_void,
            error: *mut *const c_void,
        ) -> u8;
    }
}

/// Space available for important usage on the volume holding `path` in bytes:
/// what Finder and System Settings call "Available" (free space plus
/// purgeable space). `None` when macOS does not report it.
pub fn available_for_important_usage(path: &Path) -> Option<u64> {
    use core_foundation::base::TCFType;
    use core_foundation::number::CFNumber;
    use core_foundation::url::CFURL;
    let url = CFURL::from_path(path, true)?;
    let mut value: *const std::ffi::c_void = std::ptr::null();
    // SAFETY: `url` is a live CFURL, the key is a CoreFoundation constant and
    // `value` receives a +1 retained CFNumber on success.
    let ok = unsafe {
        cf::CFURLCopyResourcePropertyForKey(
            url.as_concrete_TypeRef().cast(),
            cf::kCFURLVolumeAvailableCapacityForImportantUsageKey,
            (&mut value as *mut *const std::ffi::c_void).cast(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || value.is_null() {
        return None;
    }
    // SAFETY: the property is documented to be a CFNumber; we own the +1 reference.
    let number = unsafe { CFNumber::wrap_under_create_rule(value.cast()) };
    number.to_i64().and_then(|n| u64::try_from(n).ok())
}

/// True when `path` lives on the startup disk (the volume the disk tiles
/// describe), so a share "of used space" is meaningful.
pub fn on_startup_disk(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    ["/System/Volumes/Data", "/"]
        .iter()
        .any(|p| std::fs::metadata(p).is_ok_and(|m| m.dev() == meta.dev()))
}

// MARK: - Permissions

/// True when Full Disk Access appears granted: a TCC-protected folder could be
/// listed. `None` when there was nothing to probe.
pub fn has_full_disk_access() -> Option<bool> {
    let h = home();
    let probes = [
        "Library/Safari",
        "Library/Mail",
        "Library/Messages",
        "Library/Cookies",
        "Library/Application Support/com.apple.TCC",
    ];
    let mut saw_denial = false;
    for rel in probes {
        let dir = h.join(rel);
        if !dir.exists() {
            continue;
        }
        match std::fs::read_dir(&dir) {
            Ok(mut rd) => {
                if rd.next().is_some() {
                    return Some(true);
                }
            }
            Err(_) => saw_denial = true,
        }
    }
    if saw_denial {
        Some(false)
    } else {
        None
    }
}

/// System Settings pane for Full Disk Access.
pub const FULL_DISK_ACCESS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";

// MARK: - Display helpers

/// Path for display with the home folder shown as `~`.
pub fn display_path(path: &Path) -> String {
    let h = home();
    match path.strip_prefix(&h) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Short name of a folder: "Home" for the home folder, the volume root as
/// "Macintosh HD", else the last component.
pub fn folder_name(path: &Path) -> String {
    if standardize(path) == standardize(home()) {
        return "Home".into();
    }
    match path.file_name() {
        Some(n) => n.to_string_lossy().into_owned(),
        None => "Macintosh HD".into(),
    }
}
