//! Windows implementation of the platform layer.

use super::FileKey;
use crate::disk::DiskSpace;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf, Prefix};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, HANDLE, INVALID_HANDLE_VALUE, NO_ERROR,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetCompressedFileSizeW, GetDiskFreeSpaceExW, GetFileInformationByHandle,
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, INVALID_FILE_SIZE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// Attributes of files whose on-disk allocation differs from their length:
/// sparse, NTFS-compressed, offline and cloud placeholders (OneDrive).
const ALLOCATION_DIFFERS: u32 = 0x200 | 0x800 | 0x1000 | 0x40_0000;
/// `GetExitCodeProcess` result of a process that has not exited.
const STILL_ACTIVE: u32 = 259;

/// NUL-terminated UTF-16. Long absolute paths get the `\\?\` prefix so the
/// Win32 calls below are not limited to `MAX_PATH`.
fn wide(path: &Path) -> Vec<u16> {
    let raw: Vec<u16> = path.as_os_str().encode_wide().collect();
    let mut out = Vec::with_capacity(raw.len() + 5);
    let verbatim = raw.starts_with(&[b'\\' as u16, b'\\' as u16]);
    if raw.len() >= 240 && path.is_absolute() && !verbatim {
        out.extend(r"\\?\".encode_utf16());
    }
    out.extend(raw);
    out.push(0);
    out
}

fn wide_str(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Closes the handle on drop.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is valid and owned by this value.
        unsafe { CloseHandle(self.0) };
    }
}

// MARK: - Locations

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

pub fn home() -> PathBuf {
    env_dir("USERPROFILE").unwrap_or_else(|| PathBuf::from(r"C:\"))
}

/// `%APPDATA%` (roaming application data).
pub fn roaming_app_data() -> PathBuf {
    env_dir("APPDATA").unwrap_or_else(|| home().join(r"AppData\Roaming"))
}

/// `%LOCALAPPDATA%` (per-machine application data and caches).
pub fn local_app_data() -> PathBuf {
    env_dir("LOCALAPPDATA").unwrap_or_else(|| home().join(r"AppData\Local"))
}

pub fn app_support_dir() -> PathBuf {
    roaming_app_data().join("Nera Cleaner")
}

pub fn app_cache_dir() -> PathBuf {
    local_app_data().join("Nera Cleaner").join("Cache")
}

/// Absolute, no `.`/`..`, no trailing separator, `\\?\` removed and the drive
/// letter upper-cased. A path without a drive is rooted at `\`.
pub fn standardize(path: &Path) -> PathBuf {
    use std::path::Component::{CurDir, Normal, ParentDir, RootDir};
    let mut out = std::ffi::OsString::new();
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for comp in path.components() {
        match comp {
            std::path::Component::Prefix(p) => match p.kind() {
                Prefix::Disk(d) | Prefix::VerbatimDisk(d) => {
                    out.push(format!("{}:", (d as char).to_ascii_uppercase()));
                }
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                    out.push(r"\\");
                    out.push(server);
                    out.push(r"\");
                    out.push(share);
                }
                _ => out.push(p.as_os_str()),
            },
            RootDir | CurDir => {}
            ParentDir => {
                parts.pop();
            }
            Normal(c) => parts.push(c),
        }
    }
    out.push(r"\");
    let mut out = PathBuf::from(out);
    out.extend(parts);
    out
}

/// Component-wise prefix test, ignoring ASCII case like NTFS does.
pub fn starts_with(path: &Path, prefix: &Path) -> bool {
    let mut path = path.components();
    prefix.components().all(|want| {
        path.next()
            .is_some_and(|got| got.as_os_str().eq_ignore_ascii_case(want.as_os_str()))
    })
}

// MARK: - Sizes and file identity

/// Logical length: Windows metadata carries no allocation figure. Prefer
/// [`allocated_size_at`], which is exact for sparse, compressed and cloud files.
pub fn allocated_size(meta: &std::fs::Metadata) -> u64 {
    if meta.is_dir() {
        0
    } else {
        meta.len()
    }
}

/// Bytes the file occupies on disk. Ordinary files report their length, as
/// Explorer's "Size" does; sparse, compressed and cloud-only files report
/// their real allocation (0 for a OneDrive placeholder).
pub fn allocated_size_at(path: &Path, meta: &std::fs::Metadata) -> u64 {
    // Directories and links hold no data of their own.
    if !meta.is_file() {
        return 0;
    }
    if meta.file_attributes() & ALLOCATION_DIFFERS == 0 {
        return meta.len();
    }
    let name = wide(path);
    let mut high = 0u32;
    // SAFETY: NUL-terminated path and a valid out-pointer.
    let low = unsafe { GetCompressedFileSizeW(name.as_ptr(), &mut high) };
    // SAFETY: reads the calling thread's last error only.
    if low == INVALID_FILE_SIZE && unsafe { GetLastError() } != NO_ERROR {
        return meta.len();
    }
    (u64::from(high) << 32) | u64::from(low)
}

/// Open for attribute queries only (no read access needed), without
/// following links.
fn open_for_info(path: &Path) -> Option<Handle> {
    let name = wide(path);
    // SAFETY: NUL-terminated path; the other pointers may be null.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    (handle != INVALID_HANDLE_VALUE && !handle.is_null()).then(|| Handle(handle))
}

/// Volume serial number, file index and hard-link count. Opens the file, so
/// it costs one extra system call round trip per item.
pub fn file_key(path: &Path, _meta: &std::fs::Metadata) -> Option<FileKey> {
    let handle = open_for_info(path)?;
    // SAFETY: zero is a valid value for this plain-data struct.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: valid handle and out-pointer.
    if unsafe { GetFileInformationByHandle(handle.0, &mut info) } == 0 {
        return None;
    }
    Some(FileKey {
        volume: u64::from(info.dwVolumeSerialNumber),
        index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        links: u64::from(info.nNumberOfLinks),
    })
}

/// Unused on Windows: other volumes are only reachable through reparse
/// points, which [`may_enter`] rejects.
pub fn walk_volumes(_root: &Path) -> Vec<u64> {
    Vec::new()
}

/// Junctions, mount points and directory symlinks are never entered.
pub fn may_enter(meta: &std::fs::Metadata, _volumes: &[u64]) -> bool {
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
}

// MARK: - Recycle Bin

pub fn trash(path: &Path) -> std::io::Result<()> {
    trash::delete(path).map_err(|e| std::io::Error::other(e.to_string()))
}

// MARK: - Processes

fn open_process(pid: i32) -> Result<Handle, u32> {
    // SAFETY: plain call; a null handle signals failure.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32) };
    if handle.is_null() {
        // SAFETY: reads the calling thread's last error only.
        Err(unsafe { GetLastError() })
    } else {
        Ok(Handle(handle))
    }
}

pub fn executable_path(pid: i32) -> Option<PathBuf> {
    let handle = open_process(pid).ok()?;
    let mut buf = vec![0u16; 32_768];
    let mut len = buf.len() as u32;
    // SAFETY: valid handle; `len` holds the buffer capacity in characters.
    if unsafe { QueryFullProcessImageNameW(handle.0, 0, buf.as_mut_ptr(), &mut len) } == 0 {
        return None;
    }
    Some(PathBuf::from(std::ffi::OsString::from_wide(
        &buf[..len as usize],
    )))
}

pub fn all_pids() -> Vec<i32> {
    // SAFETY: plain call; INVALID_HANDLE_VALUE signals failure.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let snapshot = Handle(snapshot);
    // SAFETY: zero is a valid value for this plain-data struct.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut pids = Vec::new();
    // SAFETY: valid snapshot handle and an entry with dwSize set.
    let mut more = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
    while more {
        // PID 0 is the idle pseudo-process.
        if entry.th32ProcessID != 0 {
            pids.push(entry.th32ProcessID as i32);
        }
        // SAFETY: as above.
        more = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
    }
    pids
}

/// A process we may not open (access denied) exists, so it counts as alive.
pub fn process_alive(pid: i32) -> bool {
    match open_process(pid) {
        Ok(handle) => {
            let mut code = 0u32;
            // SAFETY: valid handle and out-pointer.
            unsafe { GetExitCodeProcess(handle.0, &mut code) != 0 && code == STILL_ACTIVE }
        }
        Err(code) => code == ERROR_ACCESS_DENIED,
    }
}

// MARK: - Shell

pub fn reveal(path: &Path) {
    use std::os::windows::process::CommandExt;
    // Explorer parses its own command line: the path must follow `/select,`
    // in one quoted piece (Windows file names cannot contain a quote).
    let _ = std::process::Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn();
}

pub fn open(target: &std::ffi::OsStr) {
    let verb = wide_str(std::ffi::OsStr::new("open"));
    let target = wide_str(target);
    // SAFETY: NUL-terminated strings; the remaining pointers may be null.
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

// MARK: - Disk

/// Drive Windows is installed on, with a trailing separator (`C:\`).
pub fn system_drive() -> PathBuf {
    let drive = std::env::var("SystemDrive")
        .ok()
        .filter(|d| d.len() == 2 && d.ends_with(':'))
        .unwrap_or_else(|| "C:".into());
    PathBuf::from(format!("{drive}\\"))
}

/// Capacity of the system drive: the "free" and "total" Explorer shows.
pub fn startup_disk() -> anyhow::Result<DiskSpace> {
    let root = wide(&system_drive());
    let (mut available, mut total) = (0u64, 0u64);
    // SAFETY: NUL-terminated path and valid out-pointers.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            root.as_ptr(),
            &mut available,
            &mut total,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        anyhow::bail!(
            "GetDiskFreeSpaceExW failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(DiskSpace {
        total,
        free: available,
    })
}

/// Windows has no purgeable-space figure.
pub fn important_usage() -> Option<u64> {
    None
}

/// `path` relative to `prefix`, when it lies inside it (ignoring ASCII case).
pub fn strip_prefix(path: &Path, prefix: &Path) -> Option<PathBuf> {
    starts_with(path, prefix).then(|| {
        path.components()
            .skip(prefix.components().count())
            .collect()
    })
}
