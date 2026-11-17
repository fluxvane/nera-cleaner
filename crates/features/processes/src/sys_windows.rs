//! Thin wrappers over the Windows APIs the processes engine needs. The
//! counters come from one `NtQuerySystemInformation(SystemProcessInformation)`
//! snapshot, the same source Task Manager reads, which covers every process
//! (protected and other accounts' ones included) without opening it.
//! Owners and executable paths need the process opened; failures mean
//! "Windows doesn't share this", never a made-up value.
//!
//! Everything is read-only except [`Process::terminate`] and
//! [`close_windows`], which the engine calls only after its quit policy and
//! identity checks.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows_sys::Wdk::System::SystemInformation::{
    NtQuerySystemInformation, SystemProcessInformation,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_INSUFFICIENT_BUFFER, FILETIME, HANDLE, HWND,
    LPARAM, STATUS_INFO_LENGTH_MISMATCH, UNICODE_STRING, WAIT_OBJECT_0,
};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSidToSidW};
use windows_sys::Win32::Security::{
    GetTokenInformation, LookupAccountSidW, TokenUser, PSID, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows_sys::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue,
    PdhOpenQueryW, PDH_FMT_COUNTERVALUE, PDH_FMT_LARGE,
};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{
    GetActiveProcessorCount, GetProcessTimes, GetSystemTimes, OpenProcess, OpenProcessToken,
    QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject, ALL_PROCESSOR_GROUPS,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GetClassNameW, GetWindow, GetWindowLongW,
    GetWindowThreadProcessId, IsWindowVisible, PostMessageW, GWL_EXSTYLE, GW_OWNER, WM_CLOSE,
    WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};

fn wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// Reads a NUL-terminated UTF-16 string.
///
/// # Safety
/// `p` must be null or point to a NUL-terminated UTF-16 string.
unsafe fn from_wide_ptr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0;
    while *p.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
}

fn filetime(ft: FILETIME) -> u64 {
    (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
}

/// FILETIME ticks (100 ns since 1601) to seconds since the Unix epoch.
pub fn filetime_to_unix(ticks: u64) -> u64 {
    (ticks / 10_000_000).saturating_sub(11_644_473_600)
}

// MARK: - Process snapshot

/// `SYSTEM_PROCESS_INFORMATION` with the fields `winternl.h` leaves
/// "reserved" spelled out (the layout every process tool relies on).
#[repr(C)]
#[derive(Clone, Copy)]
struct RawProcess {
    next_entry_offset: u32,
    _number_of_threads: u32,
    /// Task Manager's "Memory (private working set)".
    working_set_private_size: i64,
    _hard_fault_count: u32,
    _number_of_threads_high_watermark: u32,
    _cycle_time: u64,
    create_time: i64,
    user_time: i64,
    kernel_time: i64,
    image_name: UNICODE_STRING,
    _base_priority: i32,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
    _handle_count: u32,
    session_id: u32,
    _unique_process_key: usize,
    _peak_virtual_size: usize,
    _virtual_size: usize,
    _page_fault_count: u32,
    _peak_working_set_size: usize,
    working_set_size: usize,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::offset_of!(RawProcess, create_time) == 32);
    assert!(std::mem::offset_of!(RawProcess, image_name) == 56);
    assert!(std::mem::offset_of!(RawProcess, unique_process_id) == 80);
    assert!(std::mem::offset_of!(RawProcess, session_id) == 100);
    assert!(std::mem::offset_of!(RawProcess, working_set_size) == 144);
};

/// One process of a snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcEntry {
    pub pid: u32,
    pub parent: u32,
    /// Image name as Task Manager's Details tab shows it (`chrome.exe`,
    /// `System`, `Registry`); empty for the System Idle Process.
    pub name: String,
    /// Creation time in FILETIME ticks; with `pid` it identifies the process.
    pub create_time: u64,
    /// Kernel + user time consumed so far, in 100 ns ticks.
    pub cpu_time: u64,
    /// Private working set in bytes.
    pub private_working_set: u64,
    /// Full working set in bytes (Task Manager's "Working set (memory)").
    pub working_set: u64,
    pub session: u32,
}

/// Every process on the system, or the failing NTSTATUS.
pub fn processes() -> Result<Vec<ProcEntry>, i32> {
    let mut len: usize = 1 << 20;
    for _ in 0..8 {
        // u64 words keep the buffer aligned for the structs inside it.
        let mut buf = vec![0u64; len.div_ceil(8)];
        let bytes = buf.len() * 8;
        let mut needed = 0u32;
        // SAFETY: the buffer is valid for `bytes` bytes.
        let status = unsafe {
            NtQuerySystemInformation(
                SystemProcessInformation,
                buf.as_mut_ptr().cast::<c_void>(),
                bytes as u32,
                &mut needed,
            )
        };
        if status == STATUS_INFO_LENGTH_MISMATCH {
            // Processes may start between two calls: leave some headroom.
            len = (needed as usize).max(bytes) + (256 << 10);
            continue;
        }
        if status < 0 {
            return Err(status);
        }
        // SAFETY: the u64 buffer viewed as bytes, same length.
        let raw = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), bytes) };
        return Ok(parse_processes(raw));
    }
    Err(STATUS_INFO_LENGTH_MISMATCH)
}

/// Walks the `NextEntryOffset` chain of a `SystemProcessInformation`
/// buffer. Bounds are checked, so a short or garbled buffer only yields
/// fewer entries; names pointing outside the buffer are read as empty.
fn parse_processes(buf: &[u8]) -> Vec<ProcEntry> {
    let size = std::mem::size_of::<RawProcess>();
    let range = buf.as_ptr_range();
    let mut out = Vec::new();
    let mut offset = 0usize;
    while offset.checked_add(size).is_some_and(|end| end <= buf.len()) {
        // SAFETY: `offset + size` is within `buf`; read_unaligned has no alignment needs.
        let raw: RawProcess =
            unsafe { std::ptr::read_unaligned(buf.as_ptr().add(offset).cast::<RawProcess>()) };
        let name_ptr = raw.image_name.Buffer as *const u8;
        let name_len = usize::from(raw.image_name.Length);
        let in_buf = !name_ptr.is_null()
            && name_ptr >= range.start
            && (name_ptr as usize)
                .checked_add(name_len)
                .is_some_and(|end| end <= range.end as usize);
        let name = if in_buf && name_len > 0 {
            // SAFETY: checked above that the bytes lie inside `buf`.
            let bytes = unsafe { std::slice::from_raw_parts(name_ptr, name_len) };
            let units: Vec<u16> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            String::from_utf16_lossy(&units)
        } else {
            String::new()
        };
        out.push(ProcEntry {
            pid: raw.unique_process_id as u32,
            parent: raw.inherited_from_unique_process_id as u32,
            name,
            create_time: raw.create_time.max(0) as u64,
            cpu_time: (raw.kernel_time.max(0) as u64).saturating_add(raw.user_time.max(0) as u64),
            private_working_set: raw.working_set_private_size.max(0) as u64,
            working_set: raw.working_set_size as u64,
            session: raw.session_id,
        });
        if raw.next_entry_offset == 0 {
            break;
        }
        offset += raw.next_entry_offset as usize;
    }
    out
}

/// Logical processors in all processor groups (Task Manager's "Logical
/// processors"), the divisor of its per-process CPU column.
pub fn logical_processors() -> usize {
    // SAFETY: plain call.
    let n = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) } as usize;
    if n == 0 {
        std::thread::available_parallelism().map_or(1, |n| n.get())
    } else {
        n
    }
}

/// CPU use in % of the whole machine over a window: Task Manager's Details
/// convention (one thread spinning on a 12-processor PC reads about 8 %).
pub fn cpu_percent(delta_ticks: u64, wall_ns: u64, processors: usize) -> f32 {
    let capacity = wall_ns as f64 * processors.max(1) as f64;
    if capacity <= 0.0 {
        return 0.0;
    }
    (delta_ticks as f64 * 100.0 * 100.0 / capacity) as f32
}

/// (idle, kernel + user) system times in 100 ns ticks. Kernel time includes
/// idle time.
pub fn system_times() -> Option<(u64, u64)> {
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut idle, mut kernel, mut user) = (zero, zero, zero);
    // SAFETY: three valid out-pointers.
    if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
        return None;
    }
    Some((filetime(idle), filetime(kernel) + filetime(user)))
}

/// Busy share of all processors between two [`system_times`] readings, 0-100.
pub fn load_percent(before: (u64, u64), after: (u64, u64)) -> Option<f32> {
    let idle = after.0.checked_sub(before.0)?;
    let total = after.1.checked_sub(before.1)?;
    if total == 0 {
        return None;
    }
    Some((total.saturating_sub(idle) as f64 * 100.0 / total as f64).clamp(0.0, 100.0) as f32)
}

// MARK: - System memory

/// (usable physical memory, available) in bytes, as Task Manager's Memory
/// page shows them.
pub fn memory_status() -> Option<(u64, u64)> {
    // SAFETY: zero is valid for this plain-data struct; dwLength is set below.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: valid struct with dwLength set.
    if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 {
        return None;
    }
    Some((m.ullTotalPhys, m.ullAvailPhys))
}

/// Size of the modified page list ("Modified" in Task Manager's memory
/// composition) from the `\Memory\Modified Page List Bytes` counter.
pub fn modified_bytes() -> Option<u64> {
    struct Query(*mut c_void);
    impl Drop for Query {
        fn drop(&mut self) {
            // SAFETY: the query was opened by PdhOpenQueryW.
            unsafe { PdhCloseQuery(self.0) };
        }
    }
    let mut query = std::ptr::null_mut();
    // SAFETY: out-pointer only; a null data source means live data.
    if unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut query) } != 0 {
        return None;
    }
    let query = Query(query);
    let path = wide(r"\Memory\Modified Page List Bytes");
    let mut counter = std::ptr::null_mut();
    // SAFETY: valid query, NUL-terminated path, out-pointer.
    if unsafe { PdhAddEnglishCounterW(query.0, path.as_ptr(), 0, &mut counter) } != 0 {
        return None;
    }
    // SAFETY: valid query. A raw count needs a single collection.
    if unsafe { PdhCollectQueryData(query.0) } != 0 {
        return None;
    }
    let mut value = PDH_FMT_COUNTERVALUE::default();
    // SAFETY: valid counter and out-pointer.
    let rc = unsafe {
        PdhGetFormattedCounterValue(counter, PDH_FMT_LARGE, std::ptr::null_mut(), &mut value)
    };
    // CStatus 0 / 1: PDH_CSTATUS_VALID_DATA / PDH_CSTATUS_NEW_DATA.
    if rc != 0 || value.CStatus > 1 {
        return None;
    }
    // SAFETY: PDH_FMT_LARGE fills the i64 member.
    u64::try_from(unsafe { value.Anonymous.largeValue }).ok()
}

/// Task Manager's "In use": the physical memory that is neither available
/// (standby + free) nor waiting to be written out (modified).
pub fn in_use(total: u64, available: u64, modified: u64) -> u64 {
    total.saturating_sub(available).saturating_sub(modified)
}

// MARK: - Opening a process

/// An open process handle, closed on drop. While it is open, Windows does
/// not hand the pid to another process.
pub struct Process(HANDLE);

impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: the handle is valid and owned by this value.
        unsafe { CloseHandle(self.0) };
    }
}

/// Owner of a process: its user SID in string form (`S-1-5-21-…`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    pub sid: String,
}

impl Process {
    /// Opens `pid` with `access`, or returns the Win32 error
    /// (`ERROR_INVALID_PARAMETER`: no such process; `ERROR_ACCESS_DENIED`).
    pub fn open(pid: u32, access: u32) -> Result<Self, u32> {
        // SAFETY: plain call; a null handle signals failure.
        let handle = unsafe { OpenProcess(access, 0, pid) };
        if handle.is_null() {
            // SAFETY: reads the calling thread's last error only.
            Err(unsafe { GetLastError() })
        } else {
            Ok(Self(handle))
        }
    }

    /// Creation time in FILETIME ticks.
    pub fn creation_time(&self) -> Option<u64> {
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        // SAFETY: valid handle and four out-pointers.
        let ok =
            unsafe { GetProcessTimes(self.0, &mut created, &mut exited, &mut kernel, &mut user) };
        (ok != 0).then(|| filetime(created))
    }

    pub fn exe(&self) -> Option<PathBuf> {
        let mut buf = vec![0u16; 32_768];
        let mut len = buf.len() as u32;
        // SAFETY: valid handle; `len` holds the buffer capacity in characters.
        if unsafe { QueryFullProcessImageNameW(self.0, 0, buf.as_mut_ptr(), &mut len) } == 0 {
            return None;
        }
        Some(PathBuf::from(std::ffi::OsString::from_wide(
            &buf[..len as usize],
        )))
    }

    /// The user the process runs as, from its token. Windows only shares the
    /// token of system and service processes with elevated callers.
    pub fn owner(&self) -> Option<Owner> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: valid process handle and out-pointer.
        if unsafe { OpenProcessToken(self.0, TOKEN_QUERY, &mut token) } == 0 {
            return None;
        }
        let token = Process(token);
        let mut len = 0u32;
        // SAFETY: size query with a null buffer.
        unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut len) };
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        // SAFETY: the buffer is valid and aligned for `len` bytes.
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buf.as_mut_ptr().cast::<c_void>(),
                len,
                &mut len,
            )
        };
        if ok == 0 {
            return None;
        }
        // SAFETY: GetTokenInformation(TokenUser) wrote a TOKEN_USER at the start.
        let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
        sid_string(user.User.Sid).map(|sid| Owner { sid })
    }

    /// Waits up to `ms` milliseconds; true once the process has exited.
    /// Needs a handle opened with `PROCESS_SYNCHRONIZE`.
    pub fn wait(&self, ms: u32) -> bool {
        // SAFETY: valid handle.
        unsafe { WaitForSingleObject(self.0, ms) == WAIT_OBJECT_0 }
    }

    /// Stops the process at once (Task Manager's "End task" on a process).
    /// Needs a handle opened with `PROCESS_TERMINATE`.
    pub fn terminate(&self) -> Result<(), u32> {
        // SAFETY: valid handle.
        if unsafe { TerminateProcess(self.0, 1) } == 0 {
            // SAFETY: reads the calling thread's last error only.
            Err(unsafe { GetLastError() })
        } else {
            Ok(())
        }
    }
}

fn sid_string(sid: PSID) -> Option<String> {
    let mut out: *mut u16 = std::ptr::null_mut();
    // SAFETY: valid SID; on success `out` is a LocalAlloc'd string we free below.
    if unsafe { ConvertSidToStringSidW(sid, &mut out) } == 0 || out.is_null() {
        return None;
    }
    // SAFETY: NUL-terminated string from ConvertSidToStringSidW.
    let s = unsafe { from_wide_ptr(out) };
    // SAFETY: allocated by ConvertSidToStringSidW with LocalAlloc.
    unsafe { LocalFree(out.cast()) };
    (!s.is_empty()).then_some(s)
}

/// The SID of the account this process runs as.
pub fn current_sid() -> Option<String> {
    Process::open(
        std::process::id(),
        windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION,
    )
    .ok()?
    .owner()
    .map(|o| o.sid)
}

/// Account name of a SID, as Task Manager's "User name" column shows it
/// (`Admin`, `SYSTEM`, `LOCAL SERVICE`, `DWM-1`). `None` if unknown.
pub fn account_name(sid: &str) -> Option<String> {
    let wsid = wide(sid);
    let mut psid: PSID = std::ptr::null_mut();
    // SAFETY: NUL-terminated input; on success `psid` is LocalAlloc'd and freed below.
    if unsafe { ConvertStringSidToSidW(wsid.as_ptr(), &mut psid) } == 0 || psid.is_null() {
        return None;
    }
    let (mut name_len, mut domain_len) = (256u32, 256u32);
    let mut result = None;
    for _ in 0..2 {
        let mut name = vec![0u16; name_len as usize];
        let mut domain = vec![0u16; domain_len as usize];
        let mut kind = 0;
        // SAFETY: buffers valid for the lengths passed.
        let ok = unsafe {
            LookupAccountSidW(
                std::ptr::null(),
                psid,
                name.as_mut_ptr(),
                &mut name_len,
                domain.as_mut_ptr(),
                &mut domain_len,
                &mut kind,
            )
        };
        if ok != 0 {
            let n = String::from_utf16_lossy(&name[..name_len as usize]);
            result = (!n.is_empty()).then_some(n);
            break;
        }
        // SAFETY: reads the calling thread's last error only.
        if unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
            break;
        }
    }
    // SAFETY: allocated by ConvertStringSidToSidW with LocalAlloc.
    unsafe { LocalFree(psid) };
    result
}

/// The LocalSystem account ("SYSTEM").
pub const LOCAL_SYSTEM_SID: &str = "S-1-5-18";

/// A small stable number per SID for this run, so the engine's `uid`
/// comparisons work on Windows: LocalSystem is 0 (like root), every other
/// SID gets its own id from 1000 up.
pub fn sid_id(sid: &str) -> u32 {
    static IDS: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    let mut ids = IDS
        .get_or_init(|| Mutex::new(HashMap::from([(LOCAL_SYSTEM_SID.to_string(), 0)])))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let next = 999 + ids.len() as u32;
    *ids.entry(sid.to_ascii_uppercase()).or_insert(next)
}

// MARK: - Version resource

/// The "File description" from an executable's version resource: the name
/// Task Manager shows for apps (`Google Chrome` for `chrome.exe`).
pub fn file_description(exe: &Path) -> Option<String> {
    let path = wide(exe);
    // SAFETY: NUL-terminated path; the handle out-parameter is optional.
    let size = unsafe { GetFileVersionInfoSizeW(path.as_ptr(), std::ptr::null_mut()) };
    if size == 0 {
        return None;
    }
    let mut data = vec![0u8; size as usize];
    // SAFETY: the buffer is valid for `size` bytes.
    if unsafe { GetFileVersionInfoW(path.as_ptr(), 0, size, data.as_mut_ptr().cast()) } == 0 {
        return None;
    }
    let mut languages: Vec<(u16, u16)> = Vec::new();
    if let Some((ptr, len)) = ver_query(&data, r"\VarFileInfo\Translation") {
        // SAFETY: VerQueryValueW points into `data` with `len` bytes of (lang, codepage) pairs.
        let words = unsafe { std::slice::from_raw_parts(ptr.cast::<u16>(), len as usize / 2) };
        languages.extend(
            words
                .as_chunks::<2>()
                .0
                .iter()
                .map(|[lang, cp]| (*lang, *cp)),
        );
    }
    languages.extend([(0x0409, 0x04B0), (0x0409, 0x04E4), (0x0000, 0x04B0)]);
    languages.iter().find_map(|(lang, cp)| {
        let key = format!(r"\StringFileInfo\{lang:04x}{cp:04x}\FileDescription");
        let (ptr, len) = ver_query(&data, &key)?;
        // SAFETY: a string value of `len` UTF-16 units inside `data`.
        let units = unsafe { std::slice::from_raw_parts(ptr.cast::<u16>(), len as usize) };
        let s = String::from_utf16_lossy(units);
        let s = s.trim_end_matches('\0').trim();
        (!s.is_empty()).then(|| s.to_string())
    })
}

fn ver_query(data: &[u8], sub_block: &str) -> Option<(*const c_void, u32)> {
    let key = wide(sub_block);
    let mut ptr: *mut c_void = std::ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: `data` holds a version resource read by GetFileVersionInfoW.
    let ok = unsafe { VerQueryValueW(data.as_ptr().cast(), key.as_ptr(), &mut ptr, &mut len) };
    (ok != 0 && !ptr.is_null() && len > 0).then_some((ptr as *const c_void, len))
}

// MARK: - Windows (the GUI kind)

unsafe extern "system" fn collect_hwnd(hwnd: HWND, lparam: LPARAM) -> windows_sys::core::BOOL {
    // SAFETY: `lparam` is the `&mut Vec<HWND>` passed by the caller below.
    let out = unsafe { &mut *(lparam as *mut Vec<HWND>) };
    out.push(hwnd);
    1
}

fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    // SAFETY: valid window handle (or a stale one, which just yields 0).
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    pid
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: the buffer is valid for its length.
    let n = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// The desktop and taskbar themselves, which belong to File Explorer but
/// are not app windows (WM_CLOSE to the desktop opens the shutdown dialog).
const SHELL_CLASSES: &[&str] = &[
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
];

/// The rule the taskbar uses for showing a button: visible, and either
/// marked as an app window or unowned and not a tool window.
fn has_taskbar_button(hwnd: HWND) -> bool {
    // SAFETY: plain queries on a window handle.
    unsafe {
        if IsWindowVisible(hwnd) == 0 {
            return false;
        }
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let owner = GetWindow(hwnd, GW_OWNER);
        ex & WS_EX_APPWINDOW != 0 || (owner.is_null() && ex & WS_EX_TOOLWINDOW == 0)
    }
}

/// Hidden by the window manager: suspended Store apps, windows on another
/// virtual desktop and the shell's own invisible frames.
fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    // SAFETY: a u32 out-buffer of the size passed.
    let hr = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED as u32,
            (&mut cloaked as *mut u32).cast(),
            4,
        )
    };
    hr == 0 && cloaked != 0
}

/// A top-level window with a taskbar button and the process it belongs to.
struct AppWindow {
    hwnd: HWND,
    pid: u32,
    cloaked: bool,
}

/// Top-level windows that get a taskbar button. Store apps draw inside a
/// frame owned by `ApplicationFrameHost.exe`; those frames are credited to
/// the app process whose window sits inside them.
fn app_windows() -> Option<Vec<AppWindow>> {
    let mut all: Vec<HWND> = Vec::new();
    // SAFETY: the callback only pushes into `all`, which outlives the call.
    if unsafe { EnumWindows(Some(collect_hwnd), &mut all as *mut Vec<HWND> as LPARAM) } == 0 {
        return None;
    }
    let mut out = Vec::new();
    for hwnd in all {
        if !has_taskbar_button(hwnd) {
            continue;
        }
        let class = class_name(hwnd);
        if SHELL_CLASSES.contains(&class.as_str()) {
            continue;
        }
        let mut pid = window_pid(hwnd);
        if class == "ApplicationFrameWindow" {
            let mut children: Vec<HWND> = Vec::new();
            // SAFETY: as above.
            unsafe {
                EnumChildWindows(
                    hwnd,
                    Some(collect_hwnd),
                    &mut children as *mut Vec<HWND> as LPARAM,
                )
            };
            match children.into_iter().map(window_pid).find(|p| *p != pid) {
                Some(app) => pid = app,
                // An empty frame is not an app.
                None => continue,
            }
        }
        if pid != 0 {
            out.push(AppWindow {
                hwnd,
                pid,
                cloaked: is_cloaked(hwnd),
            });
        }
    }
    Some(out)
}

/// Processes with a visible taskbar window: what the engine calls apps.
pub fn app_pids() -> Option<HashSet<u32>> {
    Some(
        app_windows()?
            .into_iter()
            .filter(|w| !w.cloaked)
            .map(|w| w.pid)
            .collect(),
    )
}

/// Asks the windows of `pid` to close, like clicking their close button.
/// Returns how many windows the request was posted to.
pub fn close_windows(pid: u32) -> usize {
    app_windows()
        .unwrap_or_default()
        .into_iter()
        .filter(|w| w.pid == pid)
        // SAFETY: posting WM_CLOSE to a window handle has no memory-safety preconditions.
        .filter(|w| unsafe { PostMessageW(w.hwnd, WM_CLOSE, 0, 0) } != 0)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;

    #[test]
    fn converts_filetimes() {
        // 2023-11-14T22:13:20Z.
        assert_eq!(
            filetime_to_unix((1_700_000_000 + 11_644_473_600) * 10_000_000 + 5),
            1_700_000_000
        );
        assert_eq!(filetime_to_unix(0), 0);
    }

    #[test]
    fn cpu_percent_is_relative_to_all_processors() {
        // One full processor for one second on a 4-processor machine.
        assert_eq!(cpu_percent(10_000_000, 1_000_000_000, 4), 25.0);
        assert_eq!(cpu_percent(40_000_000, 1_000_000_000, 4), 100.0);
        assert_eq!(cpu_percent(0, 1_000_000_000, 4), 0.0);
        assert_eq!(cpu_percent(5, 0, 4), 0.0);
        assert_eq!(cpu_percent(10_000_000, 1_000_000_000, 0), 100.0);
    }

    #[test]
    fn load_is_the_busy_share_of_system_time() {
        assert_eq!(load_percent((100, 1000), (175, 1100)), Some(25.0));
        assert_eq!(load_percent((100, 1000), (100, 1000)), None);
        assert_eq!(load_percent((100, 1000), (50, 1100)), None);
    }

    #[test]
    fn in_use_excludes_available_and_modified() {
        assert_eq!(in_use(32, 20, 1), 11);
        assert_eq!(in_use(32, 30, 5), 0);
    }

    /// Builds a two-entry SystemProcessInformation buffer by hand.
    #[test]
    fn parses_a_synthetic_snapshot() {
        let size = std::mem::size_of::<RawProcess>();
        let stride = size + 32;
        let mut buf = vec![0u64; (stride * 2 + 64) / 8];
        let base = buf.as_mut_ptr().cast::<u8>();
        let name: Vec<u16> = "chrome.exe".encode_utf16().collect();
        let name_at = stride * 2;
        // SAFETY: all writes are inside `buf`.
        unsafe {
            std::ptr::copy_nonoverlapping(
                name.as_ptr().cast::<u8>(),
                base.add(name_at),
                name.len() * 2,
            );
            let mut idle: RawProcess = std::mem::zeroed();
            idle.next_entry_offset = stride as u32;
            idle.kernel_time = 7;
            std::ptr::write_unaligned(base.cast::<RawProcess>(), idle);
            let mut chrome: RawProcess = std::mem::zeroed();
            chrome.unique_process_id = 1234;
            chrome.inherited_from_unique_process_id = 99;
            chrome.create_time = 42;
            chrome.user_time = 10;
            chrome.kernel_time = 5;
            chrome.working_set_private_size = 4096;
            chrome.working_set_size = 8192;
            chrome.session_id = 1;
            chrome.image_name.Length = (name.len() * 2) as u16;
            chrome.image_name.MaximumLength = chrome.image_name.Length;
            chrome.image_name.Buffer = base.add(name_at).cast::<u16>();
            std::ptr::write_unaligned(base.add(stride).cast::<RawProcess>(), chrome);
        }
        // SAFETY: the u64 buffer viewed as bytes.
        let bytes = unsafe { std::slice::from_raw_parts(base, buf.len() * 8) };
        let list = parse_processes(bytes);
        assert_eq!(list.len(), 2);
        assert_eq!(
            (list[0].pid, list[0].name.as_str(), list[0].cpu_time),
            (0, "", 7)
        );
        assert_eq!(
            list[1],
            ProcEntry {
                pid: 1234,
                parent: 99,
                name: "chrome.exe".into(),
                create_time: 42,
                cpu_time: 15,
                private_working_set: 4096,
                working_set: 8192,
                session: 1,
            }
        );
        // A truncated buffer yields only what fits.
        assert_eq!(parse_processes(&bytes[..stride]).len(), 1);
        assert!(parse_processes(&bytes[..size - 1]).is_empty());
    }

    #[test]
    fn own_process_is_in_the_snapshot_and_readable() {
        let list = processes().expect("snapshot");
        assert!(list.len() > 10);
        let me = std::process::id();
        let own = list.iter().find(|e| e.pid == me).expect("own entry");
        assert!(own.private_working_set > 0 && own.working_set >= own.private_working_set);
        assert!(own.name.to_lowercase().ends_with(".exe"));
        let system = list.iter().find(|e| e.pid == 4).expect("System");
        assert_eq!(system.name, "System");
        let process = Process::open(me, PROCESS_QUERY_LIMITED_INFORMATION).expect("open self");
        assert_eq!(process.creation_time(), Some(own.create_time));
        let exe = process.exe().expect("exe");
        assert!(exe
            .file_name()
            .is_some_and(|f| f.to_string_lossy().eq_ignore_ascii_case(&own.name)));
        let sid = process.owner().expect("owner").sid;
        assert!(sid.starts_with("S-1-5-"), "{sid}");
        assert_eq!(current_sid().as_deref(), Some(sid.as_str()));
        assert!(account_name(&sid).is_some());
        assert!(account_name(LOCAL_SYSTEM_SID).is_some());
        assert_eq!(account_name("not a sid"), None);
        // A pid far beyond any the system hands out.
        assert!(Process::open(2_000_000_000, PROCESS_QUERY_LIMITED_INFORMATION).is_err());
    }

    #[test]
    fn sid_ids_are_stable_and_system_is_zero() {
        assert_eq!(sid_id(LOCAL_SYSTEM_SID), 0);
        let a = sid_id("S-1-5-21-1-2-3-1001");
        assert_ne!(a, 0);
        assert_eq!(sid_id("s-1-5-21-1-2-3-1001"), a);
        assert_ne!(sid_id("S-1-5-19"), a);
    }

    #[test]
    fn system_figures_are_readable() {
        let (total, available) = memory_status().expect("memory status");
        assert!(total > 0 && available <= total);
        let modified = modified_bytes().expect("modified page list counter");
        assert!(modified < total);
        assert!(logical_processors() >= 1);
        let (idle, all) = system_times().expect("system times");
        assert!(idle <= all);
    }

    #[test]
    fn reads_file_descriptions() {
        let explorer = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("explorer.exe");
        assert!(file_description(&explorer).is_some());
        assert_eq!(file_description(Path::new(r"C:\no\such\file.exe")), None);
    }
}
