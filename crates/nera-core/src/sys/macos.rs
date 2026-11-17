//! macOS implementation of the platform layer.

use super::FileKey;
use crate::disk::DiskSpace;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

// MARK: - Locations

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn app_support_dir() -> PathBuf {
    home().join("Library/Application Support/Nera Cleaner")
}

pub fn app_cache_dir() -> PathBuf {
    home().join("Library/Caches").join(crate::paths::BUNDLE_ID)
}

/// Absolute, no `.`/`..`, no trailing slash, and `/private/var` /
/// `/private/tmp` folded to `/var` / `/tmp`.
pub fn standardize(path: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for comp in path.components() {
        use std::path::Component::*;
        match comp {
            RootDir | Prefix(_) | CurDir => {}
            ParentDir => {
                out.pop();
            }
            Normal(c) => out.push(c),
        }
    }
    for (from, to) in [
        ("/private/var", "/var"),
        ("/private/tmp", "/tmp"),
        ("/private/etc", "/etc"),
    ] {
        if let Ok(rest) = out.strip_prefix(from) {
            return Path::new(to).join(rest);
        }
    }
    out
}

/// Paths are compared as written (APFS is case-preserving; the standardized
/// form is what every caller compares).
pub fn starts_with(path: &Path, prefix: &Path) -> bool {
    path.starts_with(prefix)
}

// MARK: - Sizes and file identity

/// `st_blocks` is in 512-byte units; matches APFS allocated size.
pub fn allocated_size(meta: &std::fs::Metadata) -> u64 {
    meta.blocks() * 512
}

/// `UF_COMPRESSED`: payload stored compressed in the file's metadata.
const UF_COMPRESSED: u32 = 0x20;

/// Equals [`allocated_size`] except for transparently compressed files, where
/// `st_blocks` says one block but the real allocation (`ATTR_FILE_ALLOCSIZE`)
/// is often 0.
pub fn allocated_size_at(path: &Path, meta: &std::fs::Metadata) -> u64 {
    use std::os::macos::fs::MetadataExt as _;
    let blocks = allocated_size(meta);
    if blocks == 0 || !meta.is_file() || meta.st_flags() & UF_COMPRESSED == 0 {
        return blocks;
    }
    attr_alloc_size(path).unwrap_or(blocks)
}

/// `ATTR_FILE_ALLOCSIZE` of a file via `getattrlist`; `None` when unavailable.
fn attr_alloc_size(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: a zeroed attrlist is a valid empty request; fields set below.
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

pub fn file_key(_path: &Path, meta: &std::fs::Metadata) -> Option<FileKey> {
    Some(FileKey {
        volume: meta.dev(),
        index: meta.ino(),
        links: meta.nlink(),
    })
}

/// Device ids a walk from `root` may enter: its own volume plus the startup
/// disk's firmlinked system and data volumes. Anything else mounted inside a
/// folder (disk images, Simulator runtimes, network shares) is skipped, like
/// `du -x`.
pub fn walk_volumes(root: &Path) -> Vec<u64> {
    let mut devs: Vec<u64> = [root, Path::new("/"), Path::new("/System/Volumes/Data")]
        .iter()
        .filter_map(|p| std::fs::symlink_metadata(p).ok().map(|m| m.dev()))
        .collect();
    devs.sort_unstable();
    devs.dedup();
    devs
}

/// True when a walk limited to `volumes` may descend into this directory.
pub fn may_enter(meta: &std::fs::Metadata, volumes: &[u64]) -> bool {
    volumes.contains(&meta.dev())
}

// MARK: - Trash

pub fn trash(path: &Path) -> std::io::Result<()> {
    use trash::macos::{DeleteMethod, TrashContextExtMacos};
    let mut ctx = trash::TrashContext::default();
    ctx.set_delete_method(DeleteMethod::NsFileManager);
    ctx.delete(path)
        .map_err(|e| std::io::Error::other(e.to_string()))
}

// MARK: - Processes

pub fn executable_path(pid: i32) -> Option<PathBuf> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: buffer is valid for its full length.
    let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if len <= 0 {
        return None;
    }
    buf.truncate(len as usize);
    Some(PathBuf::from(String::from_utf8_lossy(&buf).into_owned()))
}

/// Retries with a larger buffer when processes start between sizing and
/// filling it, so none are missed.
pub fn all_pids() -> Vec<i32> {
    // SAFETY: the buffer is valid for the byte length passed.
    unsafe {
        let n = libc::proc_listallpids(std::ptr::null_mut(), 0);
        if n <= 0 {
            return Vec::new();
        }
        let mut cap = n as usize + 64;
        for _ in 0..8 {
            let mut pids = vec![0i32; cap];
            let bytes = (pids.len() * std::mem::size_of::<i32>()) as i32;
            let got = libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes);
            if got <= 0 {
                return Vec::new();
            }
            if (got as usize) < cap {
                pids.truncate(got as usize);
                pids.retain(|p| *p > 0);
                return pids;
            }
            cap *= 2;
        }
        Vec::new()
    }
}

pub fn process_alive(pid: i32) -> bool {
    // SAFETY: kill with signal 0 only checks for existence/permission.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

// MARK: - Shell

pub fn reveal(path: &Path) {
    let _ = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn();
}

pub fn open(target: &std::ffi::OsStr) {
    let _ = std::process::Command::new("open").arg(target).spawn();
}

// MARK: - Disk

/// Raw capacity of the startup disk's data volume from `statfs` (what `df`
/// and `diskutil` call free: space unused right now, without purgeable space).
pub fn startup_disk() -> anyhow::Result<DiskSpace> {
    for path in ["/System/Volumes/Data", "/"] {
        let c = std::ffi::CString::new(path)?;
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: valid C string and out-pointer.
        if unsafe { libc::statfs(c.as_ptr(), &mut st) } == 0 {
            let bsize = st.f_bsize as u64;
            return Ok(DiskSpace {
                total: st.f_blocks * bsize,
                free: st.f_bavail * bsize,
            });
        }
    }
    anyhow::bail!("statfs failed: {}", std::io::Error::last_os_error())
}

/// Finder's "available" figure (free plus purgeable) for the startup disk.
pub fn important_usage() -> Option<u64> {
    available_for_important_usage("/System/Volumes/Data")
        .or_else(|| available_for_important_usage("/"))
}

mod cf {
    use std::ffi::c_void;
    pub type CFTypeRef = *const c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub static kCFAllocatorDefault: CFTypeRef;
        pub static kCFURLVolumeAvailableCapacityForImportantUsageKey: CFTypeRef;
        pub fn CFURLCreateFromFileSystemRepresentation(
            a: CFTypeRef,
            buf: *const u8,
            len: isize,
            is_dir: u8,
        ) -> CFTypeRef;
        pub fn CFURLCopyResourcePropertyForKey(
            url: CFTypeRef,
            key: CFTypeRef,
            out: *mut CFTypeRef,
            err: *mut CFTypeRef,
        ) -> u8;
        pub fn CFGetTypeID(v: CFTypeRef) -> usize;
        pub fn CFNumberGetTypeID() -> usize;
        pub fn CFNumberGetValue(n: CFTypeRef, kind: isize, out: *mut c_void) -> u8;
        pub fn CFRelease(v: CFTypeRef);
    }
    pub const K_CF_NUMBER_SINT64: isize = 4;
}

/// `NSURLVolumeAvailableCapacityForImportantUsageKey` of the volume holding `path`.
fn available_for_important_usage(path: &str) -> Option<u64> {
    // SAFETY: CF create rule — every object we create or copy is released.
    unsafe {
        let url = cf::CFURLCreateFromFileSystemRepresentation(
            cf::kCFAllocatorDefault,
            path.as_ptr(),
            path.len() as isize,
            1,
        );
        if url.is_null() {
            return None;
        }
        let mut value: cf::CFTypeRef = std::ptr::null();
        let ok = cf::CFURLCopyResourcePropertyForKey(
            url,
            cf::kCFURLVolumeAvailableCapacityForImportantUsageKey,
            &mut value,
            std::ptr::null_mut(),
        );
        cf::CFRelease(url);
        if ok == 0 || value.is_null() {
            return None;
        }
        let mut n: i64 = -1;
        let got = cf::CFGetTypeID(value) == cf::CFNumberGetTypeID()
            && cf::CFNumberGetValue(value, cf::K_CF_NUMBER_SINT64, (&mut n as *mut i64).cast())
                != 0;
        cf::CFRelease(value);
        (got && n >= 0).then_some(n as u64)
    }
}

/// `path` relative to `prefix`, when it lies inside it.
pub fn strip_prefix(path: &Path, prefix: &Path) -> Option<PathBuf> {
    path.strip_prefix(prefix).ok().map(Path::to_path_buf)
}
