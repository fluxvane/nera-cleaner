//! Windows startup engine: the apps Task Manager lists under Startup apps
//! (and Settings > Apps > Startup).
//!
//! Sources, all read-only while scanning:
//! - `Software\Microsoft\Windows\CurrentVersion\Run` in HKCU and HKLM, plus the
//!   32-bit `WOW6432Node` copies;
//! - the Startup folders: yours under `%APPDATA%`, everyone's under `%ProgramData%`;
//! - startup tasks of packaged (Microsoft Store) apps, listed read-only.
//!
//! On/off state lives in `...\Explorer\StartupApproved\{Run,Run32,StartupFolder}`:
//! a binary value named like the entry whose first byte is even when the item
//! is on and odd when it is off; a missing value means on. Turning an item on
//! or off writes that value exactly as Task Manager does ([`encode_approved`]).
//! Run values and shortcut files are never deleted or changed.
//!
//! Services and scheduled tasks that run at logon are a different category
//! and are not listed.
//!
//! Changes are planned by [`plan_toggle`] and only run by [`execute`], so tests
//! check the plan without touching the real startup items.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// Registry hive an entry (and its on/off value) lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Hive {
    /// `HKEY_CURRENT_USER`: you can change it.
    User,
    /// `HKEY_LOCAL_MACHINE`: every user; changing it needs an administrator.
    Machine,
}

impl Hive {
    pub fn label(self) -> &'static str {
        match self {
            Hive::User => "HKCU",
            Hive::Machine => "HKLM",
        }
    }
}

/// Where Windows finds the entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// A value under `Run`; `wow64` is the 32-bit `WOW6432Node` copy.
    Run { hive: Hive, wow64: bool },
    /// A file in a Startup folder.
    Folder(Hive),
    /// A packaged app's startup task.
    Store,
}

/// The screen's sections.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    User,
    Machine,
    Store,
}

pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const RUN32_KEY: &str = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Run";
pub const APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";
const APP_MODEL: &str =
    r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel";

impl Source {
    pub fn scope(self) -> Scope {
        match self {
            Source::Run {
                hive: Hive::User, ..
            }
            | Source::Folder(Hive::User) => Scope::User,
            Source::Run {
                hive: Hive::Machine,
                ..
            }
            | Source::Folder(Hive::Machine) => Scope::Machine,
            Source::Store => Scope::Store,
        }
    }

    /// Key that holds the entries (Run sources only).
    pub fn run_key(self) -> Option<(Hive, &'static str)> {
        match self {
            Source::Run { hive, wow64: false } => Some((hive, RUN_KEY)),
            Source::Run { hive, wow64: true } => Some((hive, RUN32_KEY)),
            _ => None,
        }
    }

    /// Key (under the same hive) holding the entries' on/off values, as Task
    /// Manager uses it. `None` for Store tasks.
    pub fn approved_key(self) -> Option<(Hive, String)> {
        let (hive, sub) = match self {
            Source::Run { hive, wow64: false } => (hive, "Run"),
            Source::Run { hive, wow64: true } => (hive, "Run32"),
            Source::Folder(hive) => (hive, "StartupFolder"),
            Source::Store => return None,
        };
        Some((hive, format!(r"{APPROVED_KEY}\{sub}")))
    }

    /// Short description for the row subtitle.
    pub fn label(self) -> &'static str {
        match self {
            Source::Run { wow64: false, .. } => "Registry",
            Source::Run { wow64: true, .. } => "Registry (32-bit)",
            Source::Folder(_) => "Startup folder",
            Source::Store => "Microsoft Store app",
        }
    }
}

/// On/off state as Task Manager shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    On,
    Off,
    /// Store task turned off by group policy.
    OffByPolicy,
    /// Store task turned on by group policy.
    OnByPolicy,
}

impl State {
    pub fn enabled(self) -> bool {
        matches!(self, State::On | State::OnByPolicy)
    }
}

/// One startup app.
#[derive(Clone, Debug)]
pub struct StartupItem {
    /// Stable id: hive + key + entry, the file path, or the Store task.
    pub id: String,
    /// Registry value name, file name in the Startup folder, or Store task id.
    /// Also the name of its on/off value.
    pub entry: String,
    /// What the row shows: the program's file description (as Task Manager
    /// shows it), else the entry name.
    pub name: String,
    pub source: Source,
    /// Raw command line (Run), file path (Startup folder) or package family (Store).
    pub command: String,
    /// The program it starts, when it could be worked out.
    pub target: Option<PathBuf>,
    /// `CompanyName` from the program's version information.
    pub publisher: Option<String>,
    pub state: State,
    /// Raw `StartupApproved` value (`None` when there is none: on).
    pub approved: Option<Vec<u8>>,
    /// The program is an absolute path that does not exist.
    pub target_missing: bool,
    /// A process runs this program right now; `None` when that can't be told.
    pub running: Option<bool>,
    /// This process may write the item's on/off value.
    pub writable: bool,
}

impl StartupItem {
    pub fn enabled(&self) -> bool {
        self.state.enabled()
    }

    pub fn scope(&self) -> Scope {
        self.source.scope()
    }

    /// Can be turned on or off from Nera Cleaner.
    pub fn is_editable(&self) -> bool {
        self.writable
            && self.source != Source::Store
            && !matches!(self.state, State::OffByPolicy | State::OnByPolicy)
    }

    /// Why the switch is missing, when it is.
    pub fn read_only_reason(&self) -> Option<&'static str> {
        if self.is_editable() {
            return None;
        }
        Some(match (self.source, self.state) {
            (_, State::OffByPolicy | State::OnByPolicy) => "Set by your organization's policy.",
            (Source::Store, _) => "Turn Store apps on or off in Settings > Apps > Startup.",
            _ => "Changing startup apps for all users needs an administrator.",
        })
    }

    /// Case-insensitive search over name, entry, publisher and command.
    pub fn matches(&self, query: &str) -> bool {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return true;
        }
        [
            Some(self.name.as_str()),
            Some(self.entry.as_str()),
            self.publisher.as_deref(),
            Some(self.command.as_str()),
        ]
        .into_iter()
        .flatten()
        .any(|s| s.to_lowercase().contains(&q))
    }

    /// Path shown in the subtitle: the program, else the raw command.
    pub fn location(&self) -> String {
        match (&self.target, self.source) {
            (Some(t), _) => t.display().to_string(),
            (None, Source::Store) => String::new(),
            (None, _) => self.command.clone(),
        }
    }

    /// What "Show in File Explorer" reveals: the shortcut for Startup-folder
    /// items, the program otherwise (when it exists).
    pub fn reveal_path(&self) -> Option<PathBuf> {
        match self.source {
            Source::Folder(_) => Some(PathBuf::from(&self.command)),
            Source::Store => None,
            Source::Run { .. } => self
                .target
                .clone()
                .filter(|t| !self.target_missing && t.is_absolute()),
        }
    }
}

/// Items matching the search `query`.
pub fn visible<'a>(
    items: impl Iterator<Item = &'a StartupItem>,
    query: &str,
) -> Vec<&'a StartupItem> {
    items.filter(|i| i.matches(query)).collect()
}

/// Result of [`scan`].
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub items: Vec<StartupItem>,
    /// Places that could not be read (`where: why`).
    pub errors: Vec<String>,
}

impl Snapshot {
    pub fn in_scope(&self, scope: Scope) -> impl Iterator<Item = &StartupItem> {
        self.items.iter().filter(move |i| i.scope() == scope)
    }
}

// ---------------------------------------------------------------------------
// StartupApproved values
// ---------------------------------------------------------------------------

/// Decoded `StartupApproved` value: on/off and, when off, the FILETIME of the
/// change. `None` for an empty value (Windows treats it as on).
pub fn decode_approved(data: &[u8]) -> Option<(bool, Option<u64>)> {
    let first = *data.first()?;
    let enabled = first & 1 == 0;
    let changed = data
        .get(4..12)
        .map(|b| u64::from_le_bytes(b.try_into().expect("8 bytes")))
        .filter(|t| *t != 0);
    Some((enabled, changed))
}

/// The 12-byte value Task Manager writes: state DWORD (2 = on, 3 = off) and the
/// FILETIME of the change, all zeros when turning on.
pub fn encode_approved(enabled: bool, now_filetime: u64) -> [u8; 12] {
    let mut out = [0u8; 12];
    if enabled {
        out[0] = 2;
    } else {
        out[0] = 3;
        out[4..].copy_from_slice(&now_filetime.to_le_bytes());
    }
    out
}

/// Current time as a Windows FILETIME (100 ns since 1601-01-01 UTC).
pub fn filetime_now() -> u64 {
    const UNIX_TO_FILETIME: u64 = 116_444_736_000_000_000;
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    UNIX_TO_FILETIME + (since.as_nanos() / 100) as u64
}

/// State of an entry from its (optional) `StartupApproved` value.
pub fn state_from_approved(data: Option<&[u8]>) -> State {
    match data.and_then(decode_approved) {
        Some((false, _)) => State::Off,
        _ => State::On,
    }
}

/// Packaged startup task `State` DWORD (`Windows.ApplicationModel.StartupTaskState`).
pub fn store_state(value: u32) -> Option<State> {
    match value {
        0 | 1 => Some(State::Off),
        2 => Some(State::On),
        3 => Some(State::OffByPolicy),
        4 => Some(State::OnByPolicy),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Command lines
// ---------------------------------------------------------------------------

/// Expand `%NAME%` references with `lookup`; unknown names stay as they are
/// (like `ExpandEnvironmentStrings`).
pub fn expand_env(s: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) if end > 0 => {
                let name = &after[..end];
                match lookup(name) {
                    Some(v) => out.push_str(&v),
                    None => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// [`expand_env`] with this process's environment.
pub fn expand_env_now(s: &str) -> String {
    expand_env(s, |name| std::env::var(name).ok())
}

const EXECUTABLE_EXTENSIONS: &[&str] = &[
    ".exe", ".com", ".bat", ".cmd", ".lnk", ".vbs", ".vbe", ".js", ".jse", ".wsf", ".ps1", ".msc",
    ".cpl", ".scr", ".pif",
];

fn has_executable_extension(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    EXECUTABLE_EXTENSIONS.iter().any(|e| lower.ends_with(e))
}

/// The program a command line starts (environment variables already expanded).
///
/// Quoted: the text inside the first pair of quotes. Some installers quote
/// twice (`"\"C:\app.exe\" --flag"`); the inner path is returned. Unquoted:
/// like `CreateProcess`, each prefix ending at a space is tried and the first
/// one naming an executable wins (`C:\Program Files\App\app.exe --x`);
/// without any extension, the first word.
pub fn program_of(command: &str) -> Option<String> {
    let cmd = command.trim();
    let non_empty = |s: &str| {
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    };
    if let Some(rest) = cmd.strip_prefix('"') {
        if let Some(inner) = rest.strip_prefix("\\\"") {
            let end = inner
                .find("\\\"")
                .or_else(|| inner.find('"'))
                .unwrap_or(inner.len());
            return non_empty(&inner[..end]);
        }
        let end = rest.find('"').unwrap_or(rest.len());
        return non_empty(&rest[..end]);
    }
    if cmd.is_empty() {
        return None;
    }
    let mut ends: Vec<usize> = cmd.match_indices([' ', '\t']).map(|(i, _)| i).collect();
    ends.push(cmd.len());
    ends.iter()
        .map(|&end| &cmd[..end])
        .find(|prefix| has_executable_extension(prefix))
        .or_else(|| ends.first().map(|&end| &cmd[..end]))
        .and_then(non_empty)
}

/// True for `C:\...`, `C:/...` and UNC paths.
fn is_absolute_windows(path: &str) -> bool {
    let b = path.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
        || path.starts_with(r"\\")
}

/// Where a program named in a command line lives: absolute paths as they are,
/// bare names looked up in `search` (with `.exe` added when there is no
/// extension, as `CreateProcess` does). `None` when it can't be found.
pub fn resolve_program(program: &str, search: &[PathBuf]) -> Option<PathBuf> {
    if is_absolute_windows(program) {
        return Some(PathBuf::from(program));
    }
    if program.contains(['\\', '/']) {
        // Relative to an unknown working directory.
        return None;
    }
    let name = if Path::new(program).extension().is_some() {
        program.to_string()
    } else {
        format!("{program}.exe")
    };
    search.iter().map(|d| d.join(&name)).find(|p| p.is_file())
}

/// Folders searched for bare program names: `System32`, then `Windows`.
fn system_search_dirs() -> Vec<PathBuf> {
    let root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("windir"))
        .map(PathBuf::from)
        .unwrap_or_else(|| nera_core::paths::system_drive().join("Windows"));
    vec![root.join("System32"), root]
}

/// True only when the path really does not exist (a file we merely can't
/// open, such as an app execution alias, is not missing).
pub fn path_is_missing(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

// ---------------------------------------------------------------------------
// Shortcuts (.lnk, MS-SHLLINK)
// ---------------------------------------------------------------------------

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn ansi_z(b: &[u8], at: usize) -> Option<String> {
    let s = b.get(at..)?;
    let end = s.iter().position(|&c| c == 0)?;
    Some(String::from_utf8_lossy(&s[..end]).into_owned())
}

fn utf16_z(b: &[u8], at: usize) -> Option<String> {
    let s = b.get(at..)?;
    let units: Vec<u16> = s
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|&u| u != 0)
        .collect();
    Some(String::from_utf16_lossy(&units))
}

/// Target path stored in a shell link (`.lnk`), unexpanded: the
/// environment-variable target when the link has one, else the local base
/// path + suffix from its LinkInfo. `None` when the file is not a link or
/// points somewhere without a path (a shell folder, a network-only target).
pub fn lnk_target(b: &[u8]) -> Option<String> {
    const HAS_ID_LIST: u32 = 0x1;
    const HAS_LINK_INFO: u32 = 0x2;
    const STRING_FLAGS: [u32; 5] = [0x4, 0x8, 0x10, 0x20, 0x40];
    const IS_UNICODE: u32 = 0x80;
    const HAS_EXP_STRING: u32 = 0x200;
    const ENV_BLOCK: u32 = 0xA000_0001;

    if u32_at(b, 0)? != 0x4C {
        return None;
    }
    let flags = u32_at(b, 0x14)?;
    let mut at = 0x4C;
    if flags & HAS_ID_LIST != 0 {
        at += 2 + u16_at(b, at)? as usize;
    }
    let mut local = None;
    if flags & HAS_LINK_INFO != 0 {
        let size = u32_at(b, at)? as usize;
        let header = u32_at(b, at + 4)?;
        let info_flags = u32_at(b, at + 8)?;
        if info_flags & 1 != 0 {
            let suffix = ansi_z(b, at + u32_at(b, at + 24)? as usize).unwrap_or_default();
            let unicode_base = if header >= 0x24 {
                utf16_z(b, at + u32_at(b, at + 28)? as usize)
            } else {
                None
            };
            let base = unicode_base.or_else(|| ansi_z(b, at + u32_at(b, at + 16)? as usize));
            local = base.map(|base| base + &suffix).filter(|p| !p.is_empty());
        }
        at += size;
    }
    if flags & HAS_EXP_STRING != 0 {
        // Skip StringData to reach the ExtraData blocks.
        for f in STRING_FLAGS {
            if flags & f != 0 {
                let chars = u16_at(b, at)? as usize;
                at += 2 + if flags & IS_UNICODE != 0 {
                    chars * 2
                } else {
                    chars
                };
            }
        }
        while let Some(size) = u32_at(b, at).map(|s| s as usize).filter(|s| *s >= 8) {
            if u32_at(b, at + 4)? == ENV_BLOCK {
                let target = utf16_z(b, at + 8 + 260)
                    .filter(|s| !s.is_empty())
                    .or_else(|| ansi_z(b, at + 8).filter(|s| !s.is_empty()));
                if target.is_some() {
                    return target;
                }
            }
            at += size;
        }
    }
    local
}

// ---------------------------------------------------------------------------
// Packaged apps
// ---------------------------------------------------------------------------

/// Package family name (`Name_PublisherId`) of a package full name
/// (`Name_Version_Arch_ResourceId_PublisherId`) or of a family name.
pub fn family_of(full_name: &str) -> Option<String> {
    let mut parts = full_name.split('_');
    let name = parts.next().filter(|s| !s.is_empty())?;
    let publisher = parts.next_back().filter(|s| !s.is_empty())?;
    Some(format!("{name}_{publisher}"))
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let key = format!(" {name}=\"");
    let start = tag.find(&key)? + key.len();
    let end = tag[start..].find('"')?;
    Some(tag[start..start + end].to_string())
}

/// What the manifest says about startup task `task_id`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ManifestTask {
    pub display_name: Option<String>,
    /// Package-relative executable (the extension's, else the application's).
    pub executable: Option<String>,
    pub publisher: Option<String>,
}

/// Find startup task `task_id` in an `AppxManifest.xml`. `None` when the
/// manifest does not declare it (the task belongs to an older version).
pub fn manifest_task(xml: &str, task_id: &str) -> Option<ManifestTask> {
    let id_at = xml.find(&format!("TaskId=\"{task_id}\""))?;
    let tag_start = xml[..id_at].rfind('<')?;
    let tag_end = id_at + xml[id_at..].find('>')?;
    let task_tag = &xml[tag_start..tag_end];
    let before = &xml[..tag_start];
    fn tag_at(xml: &str, pos: usize) -> &str {
        let end = pos + xml[pos..].find('>').unwrap_or(0);
        &xml[pos..end]
    }
    let extension = before
        .rfind("Category=\"windows.startupTask\"")
        .and_then(|p| before[..p].rfind('<'))
        .map(|p| tag_at(xml, p));
    let application = before.rfind("<Application ").map(|p| tag_at(xml, p));
    let executable = extension
        .and_then(|t| attribute(t, "Executable"))
        .or_else(|| application.and_then(|t| attribute(t, "Executable")));
    let publisher = xml
        .find("<PublisherDisplayName>")
        .map(|p| p + "<PublisherDisplayName>".len())
        .and_then(|p| {
            xml[p..]
                .find("</PublisherDisplayName>")
                .map(|e| &xml[p..p + e])
        })
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.starts_with("ms-resource:"))
        .map(str::to_owned);
    Some(ManifestTask {
        display_name: attribute(task_tag, "DisplayName"),
        executable,
        publisher,
    })
}

/// `@{FullName?ms-resource://...}` source string for `SHLoadIndirectString`.
pub fn resource_source(full_name: &str, package_name: &str, value: &str) -> Option<String> {
    let rest = value.strip_prefix("ms-resource:")?;
    let uri = if let Some(path) = rest.strip_prefix("///") {
        // Empty authority: this package.
        format!("ms-resource://{package_name}/{path}")
    } else if rest.starts_with("//") {
        format!("ms-resource:{rest}")
    } else if rest.starts_with('/') {
        format!("ms-resource://{package_name}{rest}")
    } else {
        format!("ms-resource://{package_name}/Resources/{rest}")
    };
    Some(format!("@{{{full_name}?{uri}}}"))
}

// ---------------------------------------------------------------------------
// Win32 helpers
// ---------------------------------------------------------------------------

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn os_error(code: u32) -> String {
    std::io::Error::from_raw_os_error(code as i32).to_string()
}

mod reg {
    use super::{wide, Hive};
    use std::ffi::OsStr;
    use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW,
        RegQueryInfoKeyW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
        KEY_WOW64_64KEY, REG_BINARY, REG_DWORD, REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    pub use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND};
    pub use windows_sys::Win32::System::Registry::{KEY_CREATE_SUB_KEY, KEY_READ, KEY_SET_VALUE};

    pub fn root(hive: Hive) -> HKEY {
        match hive {
            Hive::User => HKEY_CURRENT_USER,
            Hive::Machine => HKEY_LOCAL_MACHINE,
        }
    }

    pub struct Value {
        pub name: String,
        pub kind: u32,
        pub data: Vec<u8>,
    }

    impl Value {
        /// `REG_SZ` / `REG_EXPAND_SZ` text (not expanded).
        pub fn string(&self) -> Option<String> {
            if self.kind != REG_SZ && self.kind != REG_EXPAND_SZ {
                return None;
            }
            let units: Vec<u16> = self
                .data
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .take_while(|&u| u != 0)
                .collect();
            Some(String::from_utf16_lossy(&units))
        }

        pub fn dword(&self) -> Option<u32> {
            if self.kind != REG_DWORD {
                return None;
            }
            Some(u32::from_le_bytes(self.data.get(..4)?.try_into().ok()?))
        }
    }

    /// An open registry key (64-bit view); closed on drop.
    pub struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: the key is open and owned by this value.
            unsafe { RegCloseKey(self.0) };
        }
    }

    impl Key {
        pub fn open(parent: HKEY, path: &str, access: u32) -> Result<Key, u32> {
            let path = wide(OsStr::new(path));
            let mut key: HKEY = std::ptr::null_mut();
            // SAFETY: NUL-terminated path, valid out pointer.
            let rc = unsafe {
                RegOpenKeyExW(parent, path.as_ptr(), 0, access | KEY_WOW64_64KEY, &mut key)
            };
            if rc == ERROR_SUCCESS {
                Ok(Key(key))
            } else {
                Err(rc)
            }
        }

        pub fn create(parent: HKEY, path: &str, access: u32) -> Result<Key, u32> {
            let path = wide(OsStr::new(path));
            let mut key: HKEY = std::ptr::null_mut();
            // SAFETY: NUL-terminated path; class, security and disposition may be null.
            let rc = unsafe {
                RegCreateKeyExW(
                    parent,
                    path.as_ptr(),
                    0,
                    std::ptr::null(),
                    REG_OPTION_NON_VOLATILE,
                    access | KEY_WOW64_64KEY,
                    std::ptr::null(),
                    &mut key,
                    std::ptr::null_mut(),
                )
            };
            if rc == ERROR_SUCCESS {
                Ok(Key(key))
            } else {
                Err(rc)
            }
        }

        pub fn raw(&self) -> HKEY {
            self.0
        }

        /// (longest subkey name, longest value name, largest value) in chars/bytes.
        fn limits(&self) -> Result<(u32, u32, u32), u32> {
            let (mut sub, mut name, mut data) = (0u32, 0u32, 0u32);
            // SAFETY: valid key; unused outputs are null.
            let rc = unsafe {
                RegQueryInfoKeyW(
                    self.0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    &mut sub,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut name,
                    &mut data,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if rc == ERROR_SUCCESS {
                Ok((sub, name, data))
            } else {
                Err(rc)
            }
        }

        pub fn values(&self) -> Result<Vec<Value>, u32> {
            let (_, max_name, max_data) = self.limits()?;
            let mut name = vec![0u16; max_name as usize + 1];
            let mut data = vec![0u8; max_data as usize + 2];
            let mut out = Vec::new();
            let mut index = 0;
            loop {
                let mut name_len = name.len() as u32;
                let mut data_len = data.len() as u32;
                let mut kind = 0u32;
                // SAFETY: buffers and their lengths match.
                let rc = unsafe {
                    RegEnumValueW(
                        self.0,
                        index,
                        name.as_mut_ptr(),
                        &mut name_len,
                        std::ptr::null(),
                        &mut kind,
                        data.as_mut_ptr(),
                        &mut data_len,
                    )
                };
                match rc {
                    ERROR_SUCCESS => {
                        out.push(Value {
                            name: String::from_utf16_lossy(&name[..name_len as usize]),
                            kind,
                            data: data[..data_len as usize].to_vec(),
                        });
                        index += 1;
                    }
                    ERROR_NO_MORE_ITEMS => return Ok(out),
                    ERROR_MORE_DATA => {
                        // Changed while reading: grow and retry this index.
                        name.resize(name.len() * 2 + 256, 0);
                        data.resize(data.len() * 2 + 1024, 0);
                    }
                    e => return Err(e),
                }
            }
        }

        pub fn subkeys(&self) -> Result<Vec<String>, u32> {
            let (max_sub, _, _) = self.limits()?;
            let mut name = vec![0u16; max_sub as usize + 1];
            let mut out = Vec::new();
            let mut index = 0;
            loop {
                let mut len = name.len() as u32;
                // SAFETY: buffer and length match; unused outputs are null.
                let rc = unsafe {
                    RegEnumKeyExW(
                        self.0,
                        index,
                        name.as_mut_ptr(),
                        &mut len,
                        std::ptr::null(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                match rc {
                    ERROR_SUCCESS => {
                        out.push(String::from_utf16_lossy(&name[..len as usize]));
                        index += 1;
                    }
                    ERROR_NO_MORE_ITEMS => return Ok(out),
                    ERROR_MORE_DATA => name.resize(name.len() * 2 + 256, 0),
                    e => return Err(e),
                }
            }
        }

        pub fn get(&self, name: &str) -> Option<Value> {
            self.values()
                .ok()?
                .into_iter()
                .find(|v| v.name.eq_ignore_ascii_case(name))
        }

        pub fn set_binary(&self, name: &str, data: &[u8]) -> Result<(), u32> {
            let name = wide(OsStr::new(name));
            // SAFETY: NUL-terminated name; data pointer and length match.
            let rc = unsafe {
                RegSetValueExW(
                    self.0,
                    name.as_ptr(),
                    0,
                    REG_BINARY,
                    data.as_ptr(),
                    data.len() as u32,
                )
            };
            if rc == ERROR_SUCCESS {
                Ok(())
            } else {
                Err(rc)
            }
        }
    }
}

/// `FileDescription` and `CompanyName` from a program's version resource.
fn version_strings(path: &Path) -> (Option<String>, Option<String>) {
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    };
    let file = wide(path.as_os_str());
    let mut handle = 0u32;
    // SAFETY: NUL-terminated path, valid out pointer.
    let size = unsafe { GetFileVersionInfoSizeW(file.as_ptr(), &mut handle) };
    if size == 0 {
        return (None, None);
    }
    // u32 storage keeps the block DWORD-aligned.
    let mut block = vec![0u32; (size as usize).div_ceil(4)];
    // SAFETY: the buffer holds at least `size` bytes.
    if unsafe { GetFileVersionInfoW(file.as_ptr(), 0, size, block.as_mut_ptr().cast()) } == 0 {
        return (None, None);
    }
    let query = |sub: &str| -> Option<(*const u8, usize)> {
        let sub = wide(OsStr::new(sub));
        let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        // SAFETY: `block` is a version resource filled above; outputs are valid.
        let ok = unsafe { VerQueryValueW(block.as_ptr().cast(), sub.as_ptr(), &mut ptr, &mut len) };
        (ok != 0 && !ptr.is_null() && len > 0).then_some((ptr as *const u8, len as usize))
    };
    let mut languages: Vec<(u16, u16)> = Vec::new();
    if let Some((ptr, len)) = query(r"\VarFileInfo\Translation") {
        // SAFETY: VerQueryValue returned `len` bytes inside `block`.
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        languages.extend(bytes.as_chunks::<4>().0.iter().map(|c| {
            (
                u16::from_le_bytes([c[0], c[1]]),
                u16::from_le_bytes([c[2], c[3]]),
            )
        }));
    }
    languages.extend([(0x0409, 0x04B0), (0x0409, 0x04E4), (0x0000, 0x04B0)]);
    let text = |name: &str| {
        languages.iter().find_map(|(lang, cp)| {
            let (ptr, len) = query(&format!(r"\StringFileInfo\{lang:04x}{cp:04x}\{name}"))?;
            // SAFETY: string values are `len` UTF-16 units inside `block`.
            let units = unsafe { std::slice::from_raw_parts(ptr as *const u16, len) };
            let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
            let s = String::from_utf16_lossy(&units[..end]).trim().to_string();
            (!s.is_empty()).then_some(s)
        })
    };
    (text("FileDescription"), text("CompanyName"))
}

/// Resolve an indirect string (`@{Package?ms-resource://...}`).
fn load_indirect(source: &str) -> Option<String> {
    use windows_sys::Win32::UI::Shell::SHLoadIndirectString;
    let src = wide(OsStr::new(source));
    let mut buf = vec![0u16; 1024];
    // SAFETY: NUL-terminated source; buffer length in chars.
    let hr = unsafe {
        SHLoadIndirectString(
            src.as_ptr(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            std::ptr::null(),
        )
    };
    if hr < 0 {
        return None;
    }
    let end = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
    let s = String::from_utf16_lossy(&buf[..end]).trim().to_string();
    (!s.is_empty()).then_some(s)
}

// ---------------------------------------------------------------------------
// Running processes
// ---------------------------------------------------------------------------

/// Programs running now, from the same process list Task Manager uses.
#[derive(Default)]
pub struct Running {
    paths: HashSet<String>,
    /// (package family, lower-case file name) of packaged processes, to match
    /// app execution aliases under `...\Microsoft\WindowsApps`.
    packaged: HashSet<(String, String)>,
}

fn path_key(path: &Path) -> String {
    nera_core::paths::standardize(path)
        .to_string_lossy()
        .to_lowercase()
}

/// Package family of a path inside `WindowsApps` (`...\WindowsApps\<package dir>\...`).
fn windows_apps_family(path: &Path) -> Option<String> {
    let mut parts = path.components().map(|c| c.as_os_str().to_string_lossy());
    parts.find(|c| c.eq_ignore_ascii_case("WindowsApps"))?;
    family_of(&parts.next()?)
}

impl Running {
    pub fn now() -> Self {
        let mut out = Running::default();
        for pid in nera_core::fs_util::all_pids() {
            if let Some(path) = nera_core::fs_util::executable_path(pid) {
                out.add(&path);
            }
        }
        out
    }

    pub fn add(&mut self, path: &Path) {
        if let (Some(family), Some(file)) = (windows_apps_family(path), path.file_name()) {
            self.packaged
                .insert((family, file.to_string_lossy().to_lowercase()));
        }
        self.paths.insert(path_key(path));
    }

    pub fn contains(&self, program: &Path) -> bool {
        if self.paths.contains(&path_key(program)) {
            return true;
        }
        match (windows_apps_family(program), program.file_name()) {
            (Some(family), Some(file)) => self
                .packaged
                .contains(&(family, file.to_string_lossy().to_lowercase())),
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

const RUN_SOURCES: [Source; 4] = [
    Source::Run {
        hive: Hive::User,
        wow64: false,
    },
    Source::Run {
        hive: Hive::User,
        wow64: true,
    },
    Source::Run {
        hive: Hive::Machine,
        wow64: false,
    },
    Source::Run {
        hive: Hive::Machine,
        wow64: true,
    },
];

/// Startup folder of a hive: yours or everyone's.
pub fn startup_folder(hive: Hive) -> PathBuf {
    let programs = PathBuf::from(r"Microsoft\Windows\Start Menu\Programs");
    match hive {
        Hive::User => nera_core::paths::roaming_app_data()
            .join(programs)
            .join("Startup"),
        Hive::Machine => std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| nera_core::paths::system_drive().join("ProgramData"))
            .join(programs)
            .join("StartUp"),
    }
}

/// `StartupApproved` values of a source: lower-case name → raw data.
fn approved_values(source: Source) -> Result<HashMap<String, Vec<u8>>, String> {
    let Some((hive, key)) = source.approved_key() else {
        return Ok(HashMap::new());
    };
    match reg::Key::open(reg::root(hive), &key, reg::KEY_READ) {
        Ok(k) => k
            .values()
            .map(|vals| {
                vals.into_iter()
                    .map(|v| (v.name.to_lowercase(), v.data))
                    .collect()
            })
            .map_err(|e| format!(r"{}\{key}: {}", hive.label(), os_error(e))),
        Err(reg::ERROR_FILE_NOT_FOUND) => Ok(HashMap::new()),
        Err(e) => Err(format!(r"{}\{key}: {}", hive.label(), os_error(e))),
    }
}

/// Whether this process may write the source's on/off values. Only opens keys
/// with write access; nothing is written.
fn can_write(source: Source) -> bool {
    let Some((hive, key)) = source.approved_key() else {
        return false;
    };
    match reg::Key::open(reg::root(hive), &key, reg::KEY_SET_VALUE) {
        Ok(_) => true,
        Err(reg::ERROR_FILE_NOT_FOUND) => {
            reg::Key::open(reg::root(hive), APPROVED_KEY, reg::KEY_CREATE_SUB_KEY).is_ok()
        }
        Err(_) => false,
    }
}

/// Fill name, publisher, missing and running from the program path.
fn describe(item: &mut StartupItem, fallback_name: &str, running: &Running) {
    item.name = fallback_name.to_string();
    let Some(target) = item.target.clone() else {
        return;
    };
    item.target_missing =
        is_absolute_windows(&target.to_string_lossy()) && path_is_missing(&target);
    if item.target_missing {
        item.running = Some(false);
        return;
    }
    let (description, company) = version_strings(&target);
    if let Some(d) = description {
        item.name = d;
    }
    item.publisher = company;
    item.running = Some(running.contains(&target));
}

fn read_run(source: Source, running: &Running, search: &[PathBuf], snap: &mut Snapshot) {
    let Some((hive, key)) = source.run_key() else {
        return;
    };
    let values = match reg::Key::open(reg::root(hive), key, reg::KEY_READ) {
        Ok(k) => match k.values() {
            Ok(v) => v,
            Err(e) => {
                snap.errors
                    .push(format!(r"{}\{key}: {}", hive.label(), os_error(e)));
                return;
            }
        },
        Err(reg::ERROR_FILE_NOT_FOUND) => return,
        Err(e) => {
            snap.errors
                .push(format!(r"{}\{key}: {}", hive.label(), os_error(e)));
            return;
        }
    };
    let approved = approved_values(source).unwrap_or_else(|e| {
        snap.errors.push(e);
        HashMap::new()
    });
    let writable = can_write(source);
    for value in values {
        // Task Manager lists named string values only.
        let Some(command) = value.string() else {
            continue;
        };
        if value.name.is_empty() || command.trim().is_empty() {
            continue;
        }
        let approved_data = approved.get(&value.name.to_lowercase()).cloned();
        let target =
            program_of(&expand_env_now(&command)).and_then(|p| resolve_program(&p, search));
        let mut item = StartupItem {
            id: format!(r"{}\{key}\{}", hive.label(), value.name),
            entry: value.name.clone(),
            name: String::new(),
            source,
            command,
            target,
            publisher: None,
            state: state_from_approved(approved_data.as_deref()),
            approved: approved_data,
            target_missing: false,
            running: None,
            writable,
        };
        describe(&mut item, &value.name, running);
        snap.items.push(item);
    }
}

fn read_startup_folder(hive: Hive, running: &Running, snap: &mut Snapshot) {
    let source = Source::Folder(hive);
    let dir = startup_folder(hive);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            snap.errors.push(format!("{}: {e}", dir.display()));
            return;
        }
    };
    let approved = approved_values(source).unwrap_or_else(|e| {
        snap.errors.push(e);
        HashMap::new()
    });
    let writable = can_write(source);
    for entry in entries.flatten() {
        let path = entry.path();
        let is_file = entry.file_type().is_ok_and(|t| t.is_file());
        let Some(file) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if !is_file || file.eq_ignore_ascii_case("desktop.ini") {
            continue;
        }
        let is_link = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("lnk"));
        let target = if is_link {
            std::fs::read(&path)
                .ok()
                .and_then(|b| lnk_target(&b))
                .map(|t| PathBuf::from(expand_env_now(&t)))
        } else {
            Some(path.clone())
        };
        let approved_data = approved.get(&file.to_lowercase()).cloned();
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.clone());
        let mut item = StartupItem {
            id: path.to_string_lossy().into_owned(),
            entry: file.clone(),
            name: String::new(),
            source,
            command: path.to_string_lossy().into_owned(),
            target,
            publisher: None,
            state: state_from_approved(approved_data.as_deref()),
            approved: approved_data,
            target_missing: false,
            running: None,
            writable,
        };
        describe(&mut item, &stem, running);
        snap.items.push(item);
    }
}

/// Installed packages: family → (full name, display name value, install folder).
fn installed_packages() -> Result<HashMap<String, (String, String, PathBuf)>, u32> {
    let repo = format!(r"{APP_MODEL}\Repository\Packages");
    let key = reg::Key::open(reg::root(Hive::User), &repo, reg::KEY_READ)?;
    let mut out = HashMap::new();
    for full in key.subkeys()? {
        let Some(family) = family_of(&full) else {
            continue;
        };
        let Ok(pkg) = reg::Key::open(key.raw(), &full, reg::KEY_READ) else {
            continue;
        };
        let display = pkg
            .get("DisplayName")
            .and_then(|v| v.string())
            .unwrap_or_default();
        let Some(root) = pkg.get("PackageRootFolder").and_then(|v| v.string()) else {
            continue;
        };
        // Several versions can be staged; keep the last by name.
        if !matches!(out.get(&family), Some((existing, _, _)) if *existing >= full) {
            out.insert(family, (full, display, PathBuf::from(root)));
        }
    }
    Ok(out)
}

fn read_store(running: &Running, snap: &mut Snapshot) {
    let base = format!(r"{APP_MODEL}\SystemAppData");
    let tasks_root = match reg::Key::open(reg::root(Hive::User), &base, reg::KEY_READ) {
        Ok(k) => k,
        Err(reg::ERROR_FILE_NOT_FOUND) => return,
        Err(e) => {
            snap.errors
                .push(format!("Store app startup tasks: {}", os_error(e)));
            return;
        }
    };
    let packages = match installed_packages() {
        Ok(p) => p,
        Err(e) => {
            snap.errors
                .push(format!("Installed Store apps: {}", os_error(e)));
            return;
        }
    };
    for family in tasks_root.subkeys().unwrap_or_default() {
        let Some((full, display, root)) = packages.get(&family) else {
            continue; // Not installed for this user.
        };
        let Ok(fam_key) = reg::Key::open(tasks_root.raw(), &family, reg::KEY_READ) else {
            continue;
        };
        let manifest = std::fs::read_to_string(root.join("AppxManifest.xml")).ok();
        let package_name = family.split('_').next().unwrap_or(&family);
        let resolve = |value: &str| -> Option<String> {
            if value.starts_with('@') {
                load_indirect(value)
            } else if value.starts_with("ms-resource:") {
                load_indirect(&resource_source(full, package_name, value)?)
            } else {
                (!value.trim().is_empty()).then(|| value.trim().to_string())
            }
        };
        for task in fam_key.subkeys().unwrap_or_default() {
            let Some(state) = reg::Key::open(fam_key.raw(), &task, reg::KEY_READ)
                .ok()
                .and_then(|k| k.get("State"))
                .and_then(|v| v.dword())
                .and_then(store_state)
            else {
                continue;
            };
            let declared = match &manifest {
                Some(xml) => match manifest_task(xml, &task) {
                    Some(t) => t,
                    None => continue, // Left over from an older version of the app.
                },
                None => ManifestTask::default(),
            };
            let name = declared
                .display_name
                .as_deref()
                .and_then(resolve)
                .or_else(|| resolve(display.as_str()))
                .unwrap_or_else(|| package_name.to_string());
            let target = declared
                .executable
                .as_deref()
                .map(|e| root.join(e.replace('/', "\\")));
            let is_running = target.as_deref().map(|t| running.contains(t));
            snap.items.push(StartupItem {
                id: format!("store:{family}!{task}"),
                entry: task.clone(),
                name,
                source: Source::Store,
                command: family.clone(),
                target,
                publisher: declared.publisher,
                state,
                approved: None,
                target_missing: false,
                running: is_running,
                writable: false,
            });
        }
    }
}

/// Every startup app Task Manager lists, with its on/off state. Read-only.
pub fn scan() -> Snapshot {
    let running = Running::now();
    let search = system_search_dirs();
    let mut snap = Snapshot::default();
    for source in RUN_SOURCES {
        read_run(source, &running, &search, &mut snap);
    }
    for hive in [Hive::User, Hive::Machine] {
        read_startup_folder(hive, &running, &mut snap);
    }
    read_store(&running, &mut snap);
    sort_items(&mut snap.items);
    snap
}

/// By name, then entry.
pub fn sort_items(items: &mut [StartupItem]) {
    items.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.entry.to_lowercase().cmp(&b.entry.to_lowercase()))
    });
}

// ---------------------------------------------------------------------------
// Changes (planned, then executed)
// ---------------------------------------------------------------------------

/// One `StartupApproved` value to write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Write {
    pub hive: Hive,
    pub key: String,
    pub value: String,
    pub data: [u8; 12],
}

/// The value Task Manager would write to turn `item` on (`enable`) or off.
pub fn plan_toggle(item: &StartupItem, enable: bool, now_filetime: u64) -> Result<Write, String> {
    if let Some(reason) = item.read_only_reason() {
        return Err(reason.to_string());
    }
    let (hive, key) = item
        .source
        .approved_key()
        .ok_or_else(|| "This item can't be changed here.".to_string())?;
    Ok(Write {
        hive,
        key,
        value: item.entry.clone(),
        data: encode_approved(enable, now_filetime),
    })
}

/// Write the planned value (creating its key if needed).
pub fn execute(write: &Write) -> Result<(), String> {
    let key = reg::Key::create(reg::root(write.hive), &write.key, reg::KEY_SET_VALUE).map_err(
        |e| match e {
            reg::ERROR_ACCESS_DENIED => {
                "Windows needs an administrator to change this.".to_string()
            }
            e => os_error(e),
        },
    )?;
    key.set_binary(&write.value, &write.data).map_err(os_error)
}

// ---------------------------------------------------------------------------
// Tests (pure logic, temp dirs and a throw-away registry key only)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nera-startup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn item(entry: &str, source: Source, state: State) -> StartupItem {
        StartupItem {
            id: format!("test:{entry}"),
            entry: entry.into(),
            name: entry.into(),
            source,
            command: format!(r"C:\Apps\{entry}.exe"),
            target: Some(PathBuf::from(format!(r"C:\Apps\{entry}.exe"))),
            publisher: None,
            state,
            approved: None,
            target_missing: false,
            running: Some(false),
            writable: !matches!(
                source,
                Source::Store
                    | Source::Run {
                        hive: Hive::Machine,
                        ..
                    }
                    | Source::Folder(Hive::Machine)
            ),
        }
    }

    const USER_RUN: Source = Source::Run {
        hive: Hive::User,
        wow64: false,
    };

    #[test]
    fn decodes_startup_approved_values() {
        // Values seen on a real machine.
        assert_eq!(
            decode_approved(&[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            Some((true, None))
        );
        assert_eq!(
            decode_approved(&[3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            Some((false, None))
        );
        // SecurityHealth in HKLM uses 06: on.
        assert_eq!(
            decode_approved(&[6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            Some((true, None))
        );
        assert_eq!(decode_approved(&[7]), Some((false, None)));
        let ft: u64 = 0x01DA_1234_5678_9ABC;
        let mut off = vec![3, 0, 0, 0];
        off.extend(ft.to_le_bytes());
        assert_eq!(decode_approved(&off), Some((false, Some(ft))));
        assert_eq!(decode_approved(&[]), None);
        assert_eq!(state_from_approved(None), State::On);
        assert_eq!(state_from_approved(Some(&[])), State::On);
        assert_eq!(state_from_approved(Some(&off)), State::Off);
    }

    #[test]
    fn encodes_like_task_manager() {
        let ft: u64 = 0x01DB_0102_0304_0506;
        assert_eq!(
            encode_approved(true, ft),
            [2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        let off = encode_approved(false, ft);
        assert_eq!(&off[..4], &[3, 0, 0, 0]);
        assert_eq!(&off[4..], &ft.to_le_bytes());
        assert_eq!(decode_approved(&off), Some((false, Some(ft))));
        assert_eq!(
            decode_approved(&encode_approved(true, ft)),
            Some((true, None))
        );
        // 2024-01-01 < now < 2100-01-01 as FILETIME.
        let now = filetime_now();
        assert!(now > 133_485_408_000_000_000 && now < 157_766_976_000_000_000);
    }

    #[test]
    fn store_task_states() {
        assert_eq!(store_state(0), Some(State::Off));
        assert_eq!(store_state(1), Some(State::Off));
        assert_eq!(store_state(2), Some(State::On));
        assert_eq!(store_state(3), Some(State::OffByPolicy));
        assert_eq!(store_state(4), Some(State::OnByPolicy));
        assert_eq!(store_state(9), None);
        assert!(State::OnByPolicy.enabled() && !State::OffByPolicy.enabled());
    }

    #[test]
    fn expands_environment_variables() {
        let env = |n: &str| match n.to_ascii_lowercase().as_str() {
            "windir" => Some(r"C:\WINDOWS".to_string()),
            "programfiles" => Some(r"C:\Program Files".to_string()),
            _ => None,
        };
        assert_eq!(
            expand_env(r"%windir%\system32\SecurityHealthSystray.exe", env),
            r"C:\WINDOWS\system32\SecurityHealthSystray.exe"
        );
        assert_eq!(
            expand_env(r#""%ProgramFiles%\App\a.exe" --x"#, env),
            r#""C:\Program Files\App\a.exe" --x"#
        );
        assert_eq!(expand_env("%NOPE%\\a.exe", env), "%NOPE%\\a.exe");
        assert_eq!(expand_env("100% done %", env), "100% done %");
        assert_eq!(expand_env("a %% b", env), "a %% b");
    }

    #[test]
    fn finds_the_program_in_command_lines() {
        let p = |s: &str| program_of(s);
        assert_eq!(
            p(r#""C:\Program Files\Microsoft OneDrive\OneDrive.exe" /background"#).as_deref(),
            Some(r"C:\Program Files\Microsoft OneDrive\OneDrive.exe")
        );
        // Unquoted path with spaces (Docker Desktop writes this).
        assert_eq!(
            p(r"C:\Program Files\Docker\Docker\Docker Desktop.exe").as_deref(),
            Some(r"C:\Program Files\Docker\Docker\Docker Desktop.exe")
        );
        assert_eq!(
            p(r"C:\Program Files\App\app.exe --minimized now").as_deref(),
            Some(r"C:\Program Files\App\app.exe")
        );
        // Quoted twice (as written by some installers).
        assert_eq!(
            p(r#""\"C:\Users\me\AppData\Local\AnthropicClaude\claude.exe\" --startup""#).as_deref(),
            Some(r"C:\Users\me\AppData\Local\AnthropicClaude\claude.exe")
        );
        assert_eq!(
            p(r"rundll32.exe C:\x\y.dll,Entry").as_deref(),
            Some("rundll32.exe")
        );
        assert_eq!(p("ctfmon").as_deref(), Some("ctfmon"));
        assert_eq!(p(r#""C:\a b\c.exe"#).as_deref(), Some(r"C:\a b\c.exe"));
        assert_eq!(p("   "), None);
        assert_eq!(p(r#""""#), None);
    }

    #[test]
    fn resolves_programs() {
        let dir = temp_dir("resolve");
        std::fs::write(dir.join("tool.exe"), b"MZ").unwrap();
        let search = vec![dir.clone()];
        assert_eq!(resolve_program("tool", &search), Some(dir.join("tool.exe")));
        assert_eq!(
            resolve_program("tool.exe", &search),
            Some(dir.join("tool.exe"))
        );
        assert_eq!(resolve_program("other.exe", &search), None);
        assert_eq!(resolve_program(r"sub\tool.exe", &search), None);
        assert_eq!(
            resolve_program(r"D:\x\y.exe", &search),
            Some(PathBuf::from(r"D:\x\y.exe"))
        );
        assert!(path_is_missing(&dir.join("gone.exe")));
        assert!(!path_is_missing(&dir.join("tool.exe")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn utf16z(s: &str) -> Vec<u8> {
        s.encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(|u| u.to_le_bytes())
            .collect()
    }

    fn lnk_header(flags: u32) -> Vec<u8> {
        let mut b = vec![0u8; 0x4C];
        b[0] = 0x4C;
        b[0x14..0x18].copy_from_slice(&flags.to_le_bytes());
        b
    }

    /// A link with an ID list and a LinkInfo carrying `base` (ANSI and, when
    /// `unicode`, Unicode).
    fn lnk_with_link_info(base: &str, unicode: bool) -> Vec<u8> {
        let mut b = lnk_header(0x1 | 0x2 | 0x80);
        b.extend(4u16.to_le_bytes());
        b.extend([0xAA, 0xBB, 0, 0]); // ID list payload (skipped)
        let header: u32 = if unicode { 0x24 } else { 0x1C };
        let volume = [16u32, 3, 0, 16]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<u8>>();
        let mut body = Vec::new();
        let volume_at = header;
        let ansi_at = volume_at + volume.len() as u32;
        let ansi = format!("{base}\0").into_bytes();
        let suffix_at = ansi_at + ansi.len() as u32;
        let unicode_at = suffix_at + 1;
        body.extend(&volume);
        body.extend(&ansi);
        body.push(0); // empty suffix
        if unicode {
            body.extend(utf16z(base));
        }
        let size = header + body.len() as u32;
        let mut info = Vec::new();
        for v in [size, header, 1, volume_at, ansi_at, 0, suffix_at] {
            info.extend(v.to_le_bytes());
        }
        if unicode {
            info.extend(unicode_at.to_le_bytes());
            info.extend(0u32.to_le_bytes());
        }
        info.extend(body);
        b.extend(info);
        b
    }

    #[test]
    fn reads_shortcut_targets() {
        assert_eq!(
            lnk_target(&lnk_with_link_info(r"C:\Apps\tool.exe", false)).as_deref(),
            Some(r"C:\Apps\tool.exe")
        );
        assert_eq!(
            lnk_target(&lnk_with_link_info(r"C:\Prógrams\tool.exe", true)).as_deref(),
            Some(r"C:\Prógrams\tool.exe")
        );

        // Environment-variable target (HasExpString) after a Unicode argument string.
        let mut b = lnk_header(0x20 | 0x80 | 0x200);
        let args: Vec<u8> = "-x".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        b.extend(2u16.to_le_bytes());
        b.extend(args);
        let mut block = vec![0u8; 0x314];
        block[..4].copy_from_slice(&0x314u32.to_le_bytes());
        block[4..8].copy_from_slice(&0xA000_0001u32.to_le_bytes());
        let ansi = b"%windir%\\notepad.exe";
        block[8..8 + ansi.len()].copy_from_slice(ansi);
        let wide = utf16z(r"%windir%\notepad.exe");
        block[8 + 260..8 + 260 + wide.len()].copy_from_slice(&wide);
        b.extend(block);
        b.extend(0u32.to_le_bytes()); // terminal block
        assert_eq!(lnk_target(&b).as_deref(), Some(r"%windir%\notepad.exe"));

        assert_eq!(lnk_target(b"not a link"), None);
        assert_eq!(lnk_target(&lnk_header(0)), None);
        // Truncated files are rejected, not read past their end.
        let full = lnk_with_link_info(r"C:\Apps\tool.exe", false);
        assert_eq!(lnk_target(&full[..0x50]), None);
    }

    #[test]
    fn package_names_and_manifests() {
        assert_eq!(
            family_of("Microsoft.WindowsTerminal_1.21.2361.0_x64__8wekyb3d8bbwe").as_deref(),
            Some("Microsoft.WindowsTerminal_8wekyb3d8bbwe")
        );
        assert_eq!(
            family_of("MSTeams_8wekyb3d8bbwe").as_deref(),
            Some("MSTeams_8wekyb3d8bbwe")
        );
        assert_eq!(family_of("nounderscore"), None);

        let xml = r#"<Package><Properties><DisplayName>ms-resource:AppName</DisplayName>
<PublisherDisplayName>Slack Technologies Inc.</PublisherDisplayName></Properties>
<Applications><Application Id="Slack" Executable="app\Slack.exe" EntryPoint="Windows.FullTrustApplication">
<Extensions>
<uap5:Extension Category="windows.startupTask" Executable="app\Slack.exe" EntryPoint="Windows.FullTrustApplication">
<uap5:StartupTask TaskId="SlackStartup" Enabled="true" DisplayName="Slack" /></uap5:Extension>
<uap5:Extension Category="windows.startupTask">
<uap5:StartupTask TaskId="Helper" Enabled="false" DisplayName="ms-resource:HelperName" /></uap5:Extension>
</Extensions></Application></Applications></Package>"#;
        assert_eq!(
            manifest_task(xml, "SlackStartup"),
            Some(ManifestTask {
                display_name: Some("Slack".into()),
                executable: Some(r"app\Slack.exe".into()),
                publisher: Some("Slack Technologies Inc.".into()),
            })
        );
        // No Executable on the extension: the application's.
        let helper = manifest_task(xml, "Helper").unwrap();
        assert_eq!(helper.executable.as_deref(), Some(r"app\Slack.exe"));
        assert_eq!(
            helper.display_name.as_deref(),
            Some("ms-resource:HelperName")
        );
        assert_eq!(manifest_task(xml, "Gone"), None);

        assert_eq!(
            resource_source("P_1.0_x64__abc", "P", "ms-resource:AppName").as_deref(),
            Some("@{P_1.0_x64__abc?ms-resource://P/Resources/AppName}")
        );
        assert_eq!(
            resource_source("P_1.0_x64__abc", "P", "ms-resource:///Resources/X").as_deref(),
            Some("@{P_1.0_x64__abc?ms-resource://P/Resources/X}")
        );
        assert_eq!(
            resource_source("F", "P", "ms-resource://Other/R/X").as_deref(),
            Some("@{F?ms-resource://Other/R/X}")
        );
        assert_eq!(resource_source("F", "P", "Plain"), None);
    }

    #[test]
    fn running_matches_paths_and_app_aliases() {
        let mut running = Running::default();
        running.add(Path::new(r"C:\Program Files\App\App.exe"));
        running.add(Path::new(
            r"C:\Program Files\WindowsApps\MSTeams_25153.1010.3727.5483_x64__8wekyb3d8bbwe\ms-teams.exe",
        ));
        assert!(running.contains(Path::new(r"c:\program files\app\app.exe")));
        assert!(!running.contains(Path::new(r"C:\Program Files\App\Other.exe")));
        // The Run value points at the app execution alias.
        assert!(running.contains(Path::new(
            r"C:\Users\me\AppData\Local\Microsoft\WindowsApps\MSTeams_8wekyb3d8bbwe\ms-teams.exe"
        )));
        assert!(!running.contains(Path::new(
            r"C:\Users\me\AppData\Local\Microsoft\WindowsApps\Other_8wekyb3d8bbwe\ms-teams.exe"
        )));
    }

    #[test]
    fn sources_map_to_task_manager_keys() {
        assert_eq!(
            USER_RUN.approved_key(),
            Some((Hive::User, format!(r"{APPROVED_KEY}\Run")))
        );
        assert_eq!(
            Source::Run {
                hive: Hive::Machine,
                wow64: true
            }
            .approved_key(),
            Some((Hive::Machine, format!(r"{APPROVED_KEY}\Run32")))
        );
        assert_eq!(
            Source::Folder(Hive::User).approved_key(),
            Some((Hive::User, format!(r"{APPROVED_KEY}\StartupFolder")))
        );
        assert_eq!(Source::Store.approved_key(), None);
        assert_eq!(
            Source::Run {
                hive: Hive::Machine,
                wow64: true
            }
            .run_key(),
            Some((Hive::Machine, RUN32_KEY))
        );
        assert_eq!(Source::Folder(Hive::Machine).scope(), Scope::Machine);
        assert!(startup_folder(Hive::User).ends_with(r"Start Menu\Programs\Startup"));
    }

    #[test]
    fn plans_toggles_only_where_allowed() {
        let ft = 0x01DB_0000_0000_0001;
        let off = plan_toggle(&item("Discord", USER_RUN, State::On), false, ft).unwrap();
        assert_eq!(
            off,
            Write {
                hive: Hive::User,
                key: format!(r"{APPROVED_KEY}\Run"),
                value: "Discord".into(),
                data: encode_approved(false, ft),
            }
        );
        let folder = item("Tool.lnk", Source::Folder(Hive::User), State::Off);
        let on = plan_toggle(&folder, true, ft).unwrap();
        assert_eq!(on.key, format!(r"{APPROVED_KEY}\StartupFolder"));
        assert_eq!(on.value, "Tool.lnk");
        assert_eq!(on.data, [2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

        let machine = item(
            "SecurityHealth",
            Source::Run {
                hive: Hive::Machine,
                wow64: false,
            },
            State::On,
        );
        assert!(!machine.is_editable());
        assert_eq!(
            plan_toggle(&machine, false, ft).unwrap_err(),
            "Changing startup apps for all users needs an administrator."
        );
        let mut elevated = machine.clone();
        elevated.writable = true;
        assert_eq!(
            plan_toggle(&elevated, false, ft).unwrap().hive,
            Hive::Machine
        );

        let store = item("SlackStartup", Source::Store, State::On);
        assert!(plan_toggle(&store, false, ft).is_err());
        let mut policy = item("x", USER_RUN, State::OffByPolicy);
        policy.writable = true;
        assert!(plan_toggle(&policy, true, ft).is_err());
    }

    #[test]
    fn search_and_locations() {
        let mut a = item("Discord", USER_RUN, State::On);
        a.publisher = Some("Discord Inc.".into());
        a.command = r#""C:\Users\me\AppData\Local\Discord\Update.exe" --processStart"#.into();
        assert!(a.matches("discord inc"));
        assert!(a.matches("processstart"));
        assert!(a.matches(" "));
        assert!(!a.matches("zoom"));
        assert_eq!(a.location(), r"C:\Apps\Discord.exe");
        a.target = None;
        assert_eq!(a.location(), a.command);
        assert_eq!(a.reveal_path(), None);
        let f = item("Tool.lnk", Source::Folder(Hive::User), State::On);
        assert_eq!(f.reveal_path(), Some(PathBuf::from(&f.command)));
    }

    /// Writes and reads back a value under a throw-away HKCU key (not a
    /// startup key), then deletes that key.
    #[test]
    fn executes_a_write_on_a_scratch_key() {
        use windows_sys::Win32::System::Registry::RegDeleteTreeW;
        let scratch = format!(r"Software\NeraCleanerTest-{}", std::process::id());
        let write = Write {
            hive: Hive::User,
            key: format!(r"{scratch}\StartupApproved\Run"),
            value: "Example".into(),
            data: encode_approved(false, 0x0102_0304_0506_0708),
        };
        let result = execute(&write);
        let read = reg::Key::open(reg::root(Hive::User), &write.key, reg::KEY_READ)
            .ok()
            .and_then(|k| k.get("example"))
            .map(|v| v.data);
        let name = wide(OsStr::new(&scratch));
        // SAFETY: NUL-terminated name of the key this test created.
        unsafe { RegDeleteTreeW(reg::root(Hive::User), name.as_ptr()) };
        result.unwrap();
        assert_eq!(read.as_deref(), Some(&write.data[..]));
        assert!(reg::Key::open(reg::root(Hive::User), &scratch, reg::KEY_READ).is_err());
    }

    /// Read-only scan of this PC.
    #[test]
    #[ignore]
    fn scans_real_system_read_only() {
        let snap = scan();
        for i in &snap.items {
            eprintln!(
                "{:?} {} | {} | {:?} | {:?}",
                i.source, i.entry, i.name, i.state, i.target
            );
        }
        assert!(snap.errors.is_empty(), "{:?}", snap.errors);
    }
}
