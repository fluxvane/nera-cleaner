//! Windows primitives this feature needs that `nera-core` does not offer
//! (yet): elevation, the Recycle Bin, "file in use" detection, best-effort
//! removal around files other programs hold open, and Explorer's size format.
//! Every removal still goes through `nera_core::fs_util::remove_item`.

use nera_core::fs_util;
use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_DIR_NOT_EMPTY, ERROR_LOCK_VIOLATION,
    ERROR_SHARING_VIOLATION, HANDLE, INVALID_HANDLE_VALUE, S_OK,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::{
    SHEmptyRecycleBinW, SHQueryRecycleBinW, StrFormatByteSizeEx,
    SFBS_FLAGS_TRUNCATE_UNDISPLAYED_DECIMAL_DIGITS, SHERB_NOCONFIRMATION, SHERB_NOPROGRESSUI,
    SHERB_NOSOUND, SHQUERYRBINFO,
};

const FILE_ATTRIBUTE_READONLY: u32 = 0x1;

/// True when Nera runs elevated ("as administrator"). Folders outside the
/// profile (`Windows\Temp`, the system error reports) need it.
pub fn is_elevated() -> bool {
    static ELEVATED: OnceLock<bool> = OnceLock::new();
    *ELEVATED.get_or_init(|| {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: pseudo handle of this process and a valid out-pointer.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        // SAFETY: valid token handle; the buffer is a TOKEN_ELEVATION of the stated size.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                &mut elevation as *mut TOKEN_ELEVATION as *mut c_void,
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            )
        } != 0;
        // SAFETY: the token handle is owned here.
        unsafe { CloseHandle(token) };
        ok && elevation.TokenIsElevated != 0
    })
}

// MARK: - Recycle Bin

/// Bytes and items in the Recycle Bin of every drive, as Windows reports them
/// (`SHQueryRecycleBinW`). Read-only.
pub fn recycle_bin() -> Option<(u64, u64)> {
    let mut info = SHQUERYRBINFO {
        cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: null root = every drive; `info` has cbSize set.
    let hr = unsafe { SHQueryRecycleBinW(std::ptr::null(), &mut info) };
    (hr == S_OK).then(|| (info.i64Size.max(0) as u64, info.i64NumItems.max(0) as u64))
}

/// Empty the Recycle Bin of every drive, without a dialog. Permanent.
pub fn empty_recycle_bin() -> io::Result<()> {
    // SAFETY: no owner window, null root = every drive.
    let hr = unsafe {
        SHEmptyRecycleBinW(
            std::ptr::null_mut(),
            std::ptr::null(),
            SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND,
        )
    };
    // It fails (E_UNEXPECTED) when there is nothing to empty.
    if hr >= 0 || recycle_bin().is_some_and(|(_, items)| items == 0) {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "the Recycle Bin could not be emptied ({:#010x})",
            hr as u32
        )))
    }
}

// MARK: - Files in use

/// True when `err` means another program holds `path` (or something inside
/// it) open, which Windows refuses to remove. A cleanup skips such items.
pub fn is_in_use(err: &io::Error, path: &Path) -> bool {
    match err.raw_os_error().map(|c| c as u32) {
        Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION) => return true,
        Some(ERROR_ACCESS_DENIED) => return running_image(path),
        _ => {}
    }
    // Moving to the Recycle Bin reports the shell's error as text.
    let text = err.to_string().to_ascii_lowercase();
    ["0x80070020", "0x80070021", "in use by another process"]
        .iter()
        .any(|s| text.contains(s))
}

/// A file whose deletion was refused although this user may delete it: a
/// program running from it (Windows answers "access denied" for those).
fn running_image(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.file_attributes() & FILE_ATTRIBUTE_READONLY != 0 {
        return false;
    }
    let name: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: NUL-terminated path; the other pointers may be null. Opening
    // for DELETE succeeds only when the ACL allows deleting the file.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            DELETE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return false;
    }
    // SAFETY: the handle was just opened and is owned here.
    unsafe { CloseHandle(handle) };
    true
}

/// After a failed permanent removal of `item`, remove what can go one entry at
/// a time (deepest first), so a single open file does not keep the rest of a
/// folder. `Ok` when `item` is gone in the end. Otherwise the error explains
/// what stayed: an "in use" error when only files in use stayed behind.
pub fn remove_rest(item: &Path, first: io::Error) -> io::Result<()> {
    match std::fs::symlink_metadata(item) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Ok(meta) if meta.is_dir() => {}
        _ => return Err(first),
    }
    let mut left = Leftovers::default();
    left.remove_children(item);
    match fs_util::remove_item(item) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            if left.blocker.is_none() && left.busy.is_none() {
                left.note(e, item);
            }
            Err(match (left.blocker, left.busy) {
                (Some(err), _) => err,
                // Classified per file above; reported for the whole item.
                (None, Some(_)) => in_use_error(),
                (None, None) => first,
            })
        }
    }
}

#[derive(Default)]
struct Leftovers {
    /// First failure caused by a program holding a file open.
    busy: Option<io::Error>,
    /// First failure of any other kind (permissions, unreadable folder).
    blocker: Option<io::Error>,
}

impl Leftovers {
    fn note(&mut self, err: io::Error, path: &Path) {
        let slot = if is_in_use(&err, path) {
            &mut self.busy
        } else {
            &mut self.blocker
        };
        if slot.is_none() {
            *slot = Some(err);
        }
    }

    fn remove_children(&mut self, dir: &Path) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => return self.note(e, dir),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            // Junctions and directory links report `is_symlink`, so they are
            // removed as links, never entered.
            if meta.is_dir() {
                self.remove_children(&path);
            }
            match fs_util::remove_item(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                // A folder that keeps a child: the child's failure is noted.
                Err(e) if meta.is_dir() && e.raw_os_error() == Some(ERROR_DIR_NOT_EMPTY as i32) => {
                }
                Err(e) => self.note(e, &path),
            }
        }
    }
}

/// The error reported for an item that stayed only because files in it are in use.
fn in_use_error() -> io::Error {
    io::Error::from_raw_os_error(ERROR_SHARING_VIOLATION as i32)
}

// MARK: - Sizes as File Explorer shows them

/// Bytes formatted by Windows itself (`StrFormatByteSizeEx`, the function
/// behind File Explorer's sizes): binary units labelled KB/MB/GB, three
/// significant digits with the rest cut off, in the user's locale.
pub fn explorer_bytes(n: u64) -> String {
    let mut buf = [0u16; 64];
    // SAFETY: the buffer and its length in characters are valid.
    let hr = unsafe {
        StrFormatByteSizeEx(
            n,
            SFBS_FLAGS_TRUNCATE_UNDISPLAYED_DECIMAL_DIGITS,
            buf.as_mut_ptr(),
            buf.len() as u32,
        )
    };
    if hr < 0 {
        return nera_core::format::bytes(n);
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Digits only, so the checks hold in every locale.
    fn digits(s: &str) -> String {
        s.chars().filter(|c| c.is_ascii_digit()).collect()
    }

    #[test]
    fn sizes_use_explorers_binary_units_and_truncate() {
        // 2^31 - 1 bytes is 1.99 GB in Explorer (rounding would say 2.00).
        assert_eq!(digits(&explorer_bytes(2_147_483_647)), "199");
        assert_eq!(digits(&explorer_bytes(1024 * 1024)), "100");
        // 476 GiB, a "512 GB" drive.
        assert_eq!(digits(&explorer_bytes(476 * 1024 * 1024 * 1024)), "476");
        assert_eq!(digits(&explorer_bytes(512)), "512");
    }

    #[test]
    fn queries_are_read_only_and_answer() {
        let _ = is_elevated();
        // Any answer is fine; it must not fail on a normal system.
        assert!(recycle_bin().is_some());
    }

    #[test]
    fn classifies_files_in_use() {
        let dir = std::env::temp_dir().join(format!("nera-inuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let held = dir.join("sub").join("held.bin");
        std::fs::write(&held, b"x").unwrap();
        std::fs::write(dir.join("free.bin"), b"y").unwrap();
        // Open without FILE_SHARE_DELETE, like most programs do.
        use std::os::windows::fs::OpenOptionsExt;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&held)
            .unwrap();
        let err = fs_util::remove_item(&held).unwrap_err();
        assert!(is_in_use(&err, &held), "{err:?}");
        // The rest of the folder goes; the open file stays and is reported as in use.
        let first = fs_util::remove_item(&dir).unwrap_err();
        let res = remove_rest(&dir, first);
        assert!(res.as_ref().is_err_and(|e| is_in_use(e, &dir)), "{res:?}");
        assert!(held.exists());
        assert!(!dir.join("free.bin").exists());
        drop(lock);
        assert!(remove_rest(&dir, in_use_error()).is_ok());
        assert!(!dir.exists());
        // A plain permission error is not "in use".
        let denied = io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32);
        assert!(!is_in_use(&denied, &dir.join("missing")));
    }
}
