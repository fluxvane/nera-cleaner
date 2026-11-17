//! Windows engine of the Applications screen: the desktop programs that
//! Settings > Apps > Installed apps lists, read from the registry's Uninstall
//! keys (HKLM 64-bit, HKLM 32-bit `WOW6432Node`, HKCU) with the version,
//! publisher, size and install date Settings shows for them.
//!
//! Uninstalling launches the program's own uninstaller (`UninstallString`, or
//! `msiexec /x {product code}` for Windows Installer packages). Nothing here
//! deletes files. Microsoft Store (MSIX) apps are not listed.

use chrono::NaiveDate;
use nera_core::{fs_util, paths};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// MARK: - Model

/// Which Uninstall key an entry comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    /// `HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall` (64-bit view).
    Machine64,
    /// The same key in the 32-bit view (`WOW6432Node`).
    Machine32,
    /// `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall`.
    User,
}

impl Scope {
    pub const ALL: [Scope; 3] = [Scope::Machine64, Scope::Machine32, Scope::User];

    /// Short tag used in app ids and the dump.
    pub fn tag(self) -> &'static str {
        match self {
            Scope::Machine64 => "HKLM64",
            Scope::Machine32 => "HKLM32",
            Scope::User => "HKCU",
        }
    }
}

/// Raw values of one Uninstall subkey. Text values are trimmed; empty ones are `None`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RegEntry {
    /// Subkey name (a product code for Windows Installer packages).
    pub key: String,
    pub display_name: Option<String>,
    pub display_version: Option<String>,
    pub publisher: Option<String>,
    pub install_location: Option<String>,
    /// `EstimatedSize`, in KB (1024 bytes).
    pub estimated_size_kb: Option<u32>,
    /// `InstallDate`, normally `YYYYMMDD`.
    pub install_date: Option<String>,
    pub uninstall_string: Option<String>,
    pub system_component: bool,
    pub parent_key_name: Option<String>,
    pub windows_installer: bool,
    pub no_remove: bool,
    /// When the subkey was last written.
    pub last_write: Option<SystemTime>,
}

/// Why an app may or may not be uninstalled from Nera Cleaner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protection {
    Removable,
    /// Nera Cleaner itself.
    SelfApp,
}

/// A program and its arguments exactly as Windows starts them: `args` is passed
/// on verbatim, never through a shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UninstallCommand {
    pub program: String,
    pub args: String,
}

impl UninstallCommand {
    /// The command line as one string, the program quoted when it has spaces.
    pub fn display(&self) -> String {
        let program = if self.program.contains(' ') {
            format!("\"{}\"", self.program)
        } else {
            self.program.clone()
        };
        if self.args.is_empty() {
            program
        } else {
            format!("{program} {}", self.args)
        }
    }
}

/// One program as Settings > Apps lists it. `id` is its stable id.
#[derive(Clone, Debug)]
pub struct InstalledApp {
    /// `<scope tag>\<subkey>`, e.g. `HKCU\Discord`.
    pub id: String,
    pub scope: Scope,
    pub key: String,
    pub name: String,
    pub version: String,
    pub publisher: String,
    /// `InstallLocation`, when it names an existing folder.
    pub install_location: Option<PathBuf>,
    /// The folder the program runs from, used to tell whether it is open:
    /// `InstallLocation`, else the uninstaller's own folder, and only when that
    /// folder belongs to this program alone (never `Program Files` itself,
    /// `Windows`, a package cache, ...).
    pub app_folder: Option<PathBuf>,
    /// The size Settings shows: `EstimatedSize` plus, for Windows Installer
    /// packages, the cached `.msi` Windows keeps for them. `None` when the
    /// installer reports none (Settings then shows no size either).
    pub size: Option<u64>,
    /// `InstallDate`, or the day the entry was last written when there is none.
    pub install_date: Option<NaiveDate>,
    pub uninstall: Option<UninstallCommand>,
    /// `NoRemove`: the vendor disabled uninstalling from Settings.
    pub no_remove: bool,
    pub protection: Protection,
    pub running: bool,
}

impl InstalledApp {
    /// An uninstaller can be opened for this app.
    pub fn can_uninstall(&self) -> bool {
        self.protection == Protection::Removable && !self.no_remove && self.uninstall.is_some()
    }
}

/// What identifies "this app" so it can never uninstall itself.
#[derive(Clone, Debug, Default)]
pub struct SelfIdentity {
    pub executable: Option<PathBuf>,
}

impl SelfIdentity {
    pub fn current() -> Self {
        Self {
            executable: std::env::current_exe().ok(),
        }
    }
}

/// Executable paths of running processes, captured once.
#[derive(Clone, Debug, Default)]
pub struct RunningSet {
    exes: Vec<PathBuf>,
}

impl RunningSet {
    /// Every process whose executable path this user may read.
    pub fn snapshot() -> Self {
        Self {
            exes: fs_util::all_pids()
                .into_iter()
                .filter_map(fs_util::executable_path)
                .collect(),
        }
    }

    pub fn from_paths(exes: Vec<PathBuf>) -> Self {
        Self { exes }
    }

    /// A process runs an executable inside the app's own folder.
    pub fn is_app_running(&self, app: &InstalledApp) -> bool {
        app.app_folder
            .as_ref()
            .is_some_and(|folder| self.exes.iter().any(|e| paths::is_under(e, folder)))
    }
}

/// Folders the engine needs to judge paths, taken from the environment.
#[derive(Clone, Debug)]
pub struct Env {
    pub windows_dir: PathBuf,
    pub system32: PathBuf,
    /// Shared roots that never belong to one program (`Program Files`,
    /// `%LOCALAPPDATA%\Programs`, the user's profile, ...).
    pub shared_roots: Vec<PathBuf>,
}

impl Env {
    pub fn current() -> Self {
        let var = |name: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let windows_dir = var("SystemRoot")
            .or_else(|| var("windir"))
            .unwrap_or_else(|| paths::system_drive().join("Windows"));
        let local = paths::local_app_data();
        let mut shared_roots: Vec<PathBuf> = [
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
            "CommonProgramFiles",
            "CommonProgramFiles(x86)",
            "CommonProgramW6432",
            "ProgramData",
            "ALLUSERSPROFILE",
            "PUBLIC",
        ]
        .iter()
        .filter_map(|v| var(v))
        .collect();
        let program_files = var("ProgramFiles");
        shared_roots.extend([
            paths::system_drive(),
            paths::home(),
            paths::home().join("AppData"),
            paths::roaming_app_data(),
            local.join("Programs"),
            local.join(r"Microsoft\WindowsApps"),
            local.join(r"Microsoft\WinGet\Packages"),
            local.clone(),
        ]);
        if let Some(pf) = program_files {
            shared_roots.push(pf.join("WindowsApps"));
        }
        Self {
            system32: windows_dir.join("System32"),
            windows_dir,
            shared_roots,
        }
    }
}

// MARK: - Parsing (pure)

/// `YYYYMMDD` as Windows Installer and most installers write it. Anything else
/// (Discord writes `20262820`) is no date; Settings shows none either.
pub fn parse_install_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    if s.len() != 8 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let y: i32 = s[0..4].parse().ok()?;
    let m: u32 = s[4..6].parse().ok()?;
    let d: u32 = s[6..8].parse().ok()?;
    NaiveDate::from_ymd_opt(y, m, d)
}

/// The install date Settings shows: `InstallDate` when the entry has one,
/// else the local day its key was last written.
pub fn install_date(entry: &RegEntry) -> Option<NaiveDate> {
    match &entry.install_date {
        Some(s) => parse_install_date(s),
        None => entry
            .last_write
            .map(|t| chrono::DateTime::<chrono::Local>::from(t).date_naive()),
    }
}

/// The size Settings shows: `EstimatedSize` KB, plus the cached `.msi` of a
/// Windows Installer package. No size when the installer reports none or 0.
pub fn settings_size(estimated_kb: Option<u32>, msi_package: Option<u64>) -> Option<u64> {
    let kb = estimated_kb.filter(|kb| *kb > 0)?;
    Some(u64::from(kb) * 1024 + msi_package.unwrap_or(0))
}

/// `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}` (braces included).
fn is_guid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 38
        && b[0] == b'{'
        && b[37] == b'}'
        && b[1..37].iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The product code a Windows Installer entry's key is named after.
pub fn msi_product_code(key: &str) -> Option<&str> {
    is_guid(key).then_some(key)
}

/// The first `{GUID}` inside `s`.
fn guid_in(s: &str) -> Option<&str> {
    s.match_indices('{')
        .filter_map(|(i, _)| s.get(i..i + 38))
        .find(|c| is_guid(c))
}

/// Split a command line into program and arguments, as `CreateProcess` reads
/// it. A quoted program ends at its closing quote. An unquoted one may contain
/// spaces (`C:\Program Files\Foo\uninstall.exe /S`): the shortest prefix that
/// ends at a space, names an executable and exists is the program. Otherwise
/// the first word is (`MsiExec.exe /X{...}`, `winget uninstall ...`).
pub fn parse_command(s: &str, is_file: impl Fn(&Path) -> bool) -> Option<UninstallCommand> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let cmd = |program: &str, args: &str| {
        let program = program.trim();
        (!program.is_empty()).then(|| UninstallCommand {
            program: program.to_string(),
            args: args.trim().to_string(),
        })
    };
    if let Some(rest) = s.strip_prefix('"') {
        return match rest.find('"') {
            Some(end) => cmd(&rest[..end], &rest[end + 1..]),
            None => cmd(rest, ""),
        };
    }
    let ends = s
        .match_indices(' ')
        .map(|(i, _)| i)
        .chain(std::iter::once(s.len()));
    for end in ends {
        let candidate = &s[..end];
        if candidate.contains(['\\', '/'])
            && is_executable_name(candidate)
            && is_file(Path::new(candidate))
        {
            return cmd(candidate, &s[end..]);
        }
    }
    match s.split_once(char::is_whitespace) {
        Some((program, args)) => cmd(program, args),
        None => cmd(s, ""),
    }
}

fn is_executable_name(path: &str) -> bool {
    Path::new(path).extension().is_some_and(|e| {
        ["exe", "com", "bat", "cmd"]
            .iter()
            .any(|x| e.eq_ignore_ascii_case(x))
    })
}

/// File name of `program` without `.exe`, lower-cased.
fn program_stem(program: &str) -> String {
    let name = program.rsplit(['\\', '/']).next().unwrap_or(program);
    let lower = name.to_ascii_lowercase();
    lower
        .strip_suffix(".exe")
        .map(str::to_string)
        .unwrap_or(lower)
}

/// The command that uninstalls `entry`, as Settings runs it: `msiexec /x
/// {product code}` for Windows Installer packages, else `UninstallString`.
/// Bare system tools (`MsiExec.exe`, `RunDll32.exe`) resolve to `System32`.
pub fn uninstall_command(
    entry: &RegEntry,
    env: &Env,
    is_file: impl Fn(&Path) -> bool,
) -> Option<UninstallCommand> {
    let msiexec = || env.system32.join("msiexec.exe").display().to_string();
    if entry.windows_installer {
        if let Some(code) = msi_product_code(&entry.key) {
            return Some(UninstallCommand {
                program: msiexec(),
                args: format!("/x {code}"),
            });
        }
    }
    let mut cmd = parse_command(entry.uninstall_string.as_deref()?, &is_file)?;
    if program_stem(&cmd.program) == "msiexec" {
        cmd.program = msiexec();
        if let Some(code) = guid_in(&cmd.args) {
            cmd.args = format!("/x {code}");
        }
    } else if !cmd.program.contains(['\\', '/']) {
        let mut local = env.system32.join(&cmd.program);
        if local.extension().is_none() {
            local.set_extension("exe");
        }
        if is_file(&local) {
            cmd.program = local.display().to_string();
        }
    }
    Some(cmd)
}

/// `InstallLocation` as a path: quotes and trailing separators removed.
pub fn clean_location(s: &str) -> Option<PathBuf> {
    let s = s.trim().trim_matches('"').trim();
    if s.is_empty() {
        return None;
    }
    let trimmed = s.trim_end_matches(['\\', '/']);
    // Keep the separator of a drive root ("C:\").
    let s = if trimmed.ends_with(':') { s } else { trimmed };
    let path = PathBuf::from(s);
    path.is_absolute().then_some(path)
}

/// True when `folder` can belong to one program: absolute, not a drive root,
/// not one of the shared roots, not inside `Windows` or a package cache.
pub fn is_own_folder(folder: &Path, env: &Env) -> bool {
    use std::path::Component;
    if !folder.is_absolute() {
        return false;
    }
    let mut normal = 0;
    for c in folder.components() {
        if let Component::Normal(name) = c {
            normal += 1;
            if name.eq_ignore_ascii_case("Package Cache") {
                return false;
            }
        }
    }
    normal > 0
        && !paths::is_under(folder, &env.windows_dir)
        && !env
            .shared_roots
            .iter()
            .any(|r| paths::is_under(folder, r) && paths::is_under(r, folder))
}

/// The folder a program runs from (see [`InstalledApp::app_folder`]).
fn app_folder(
    location: Option<&Path>,
    uninstall: Option<&UninstallCommand>,
    env: &Env,
    is_dir: &impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    // Shared tools whose folder never identifies one program.
    const SHARED_TOOLS: [&str; 4] = ["msiexec", "rundll32", "winget", "setup"];
    if let Some(loc) = location.filter(|l| is_own_folder(l, env) && is_dir(l)) {
        return Some(loc.to_path_buf());
    }
    let cmd = uninstall?;
    let program = Path::new(&cmd.program);
    if !program.is_absolute() || SHARED_TOOLS.contains(&program_stem(&cmd.program).as_str()) {
        return None;
    }
    program
        .parent()
        .filter(|p| is_own_folder(p, env) && is_dir(p))
        .map(Path::to_path_buf)
}

/// Turn one Uninstall subkey into a listed app, applying the rules Settings
/// uses: entries without a `DisplayName`, system components and updates of
/// other products (`ParentKeyName`) are not listed.
pub fn app_from_entry(
    scope: Scope,
    entry: &RegEntry,
    msi_package: Option<u64>,
    env: &Env,
    me: &SelfIdentity,
) -> Option<InstalledApp> {
    if entry.system_component || entry.parent_key_name.is_some() {
        return None;
    }
    let name = entry.display_name.clone()?;
    let is_file = |p: &Path| p.is_file();
    let is_dir = |p: &Path| p.is_dir();
    let uninstall = uninstall_command(entry, env, is_file);
    let location = entry
        .install_location
        .as_deref()
        .and_then(clean_location)
        .filter(|l| l.is_dir());
    let folder = app_folder(location.as_deref(), uninstall.as_ref(), env, &is_dir);
    let is_self = me
        .executable
        .as_ref()
        .zip(folder.as_ref())
        .is_some_and(|(exe, f)| paths::is_under(exe, f));
    Some(InstalledApp {
        id: format!("{}\\{}", scope.tag(), entry.key),
        scope,
        key: entry.key.clone(),
        name,
        version: entry.display_version.clone().unwrap_or_default(),
        publisher: entry.publisher.clone().unwrap_or_default(),
        install_location: location,
        app_folder: folder,
        size: settings_size(
            entry.estimated_size_kb,
            msi_package.filter(|_| entry.windows_installer),
        ),
        install_date: install_date(entry),
        uninstall,
        no_remove: entry.no_remove,
        protection: if is_self {
            Protection::SelfApp
        } else {
            Protection::Removable
        },
        running: false,
    })
}

/// Windows Installer's "packed" form of a product code, as used under
/// `Installer\UserData\<SID>\Products`.
pub fn pack_guid(code: &str) -> Option<String> {
    if !is_guid(code) {
        return None;
    }
    let hex: Vec<char> = code.chars().filter(char::is_ascii_hexdigit).collect();
    let mut out = String::with_capacity(32);
    for (start, len) in [(0, 8), (8, 4), (12, 4)] {
        out.extend(hex[start..start + len].iter().rev());
    }
    for pair in hex[16..32].chunks(2) {
        out.push(pair[1]);
        out.push(pair[0]);
    }
    Some(out.to_ascii_uppercase())
}

/// The moment a `FILETIME` (100 ns ticks since 1601, UTC) stands for.
pub fn filetime_to_system(ticks: u64) -> Option<SystemTime> {
    const UNIX_OFFSET_SECS: u64 = 11_644_473_600;
    let secs = ticks / 10_000_000;
    let nanos = (ticks % 10_000_000) * 100;
    let since_unix = secs.checked_sub(UNIX_OFFSET_SECS)?;
    Some(UNIX_EPOCH + Duration::new(since_unix, nanos as u32))
}

// MARK: - Discovery

/// Result of [`discover`].
#[derive(Debug, Default)]
pub struct Discovery {
    pub apps: Vec<InstalledApp>,
    /// Uninstall keys that exist but could not be read.
    pub unreadable: Vec<Scope>,
}

/// Every app Settings > Apps lists from the registry, sorted by name.
pub fn discover(me: &SelfIdentity, running: &RunningSet) -> Discovery {
    let env = Env::current();
    let mut out = Discovery::default();
    let mut raw: Vec<(Scope, RegEntry)> = Vec::new();
    for scope in Scope::ALL {
        match reg::read_uninstall(scope) {
            Ok(entries) => raw.extend(entries.into_iter().map(|e| (scope, e))),
            Err(reg::Missing) => {}
            Err(reg::Unreadable) => out.unreadable.push(scope),
        }
    }
    let codes: Vec<&str> = raw
        .iter()
        .filter(|(_, e)| e.windows_installer)
        .filter_map(|(_, e)| msi_product_code(&e.key))
        .collect();
    let packages = reg::msi_package_sizes(&codes);
    for (scope, entry) in &raw {
        let package = msi_product_code(&entry.key)
            .and_then(|c| packages.get(&c.to_ascii_uppercase()))
            .copied();
        if let Some(mut app) = app_from_entry(*scope, entry, package, &env, me) {
            app.running = running.is_app_running(&app);
            out.apps.push(app);
        }
    }
    out.apps.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

/// True while Windows still lists the app (its Uninstall key exists). A key
/// that cannot be opened for another reason counts as still there.
pub fn is_still_listed(scope: Scope, key: &str) -> bool {
    reg::key_exists(scope, key)
}

// MARK: - Filtering and sorting

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SortKey {
    Name,
    /// Largest first; apps without a size last.
    #[default]
    Size,
    /// Newest first; apps without a date last.
    InstallDate,
}

/// Indices of the apps matching `query` (name or publisher, case-insensitive)
/// in `sort` order.
pub fn filter_sort(apps: &[InstalledApp], query: &str, sort: SortKey) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    let mut idx: Vec<usize> = apps
        .iter()
        .enumerate()
        .filter(|(_, a)| {
            q.is_empty()
                || a.name.to_lowercase().contains(&q)
                || a.publisher.to_lowercase().contains(&q)
        })
        .map(|(i, _)| i)
        .collect();
    let name = |i: &usize| apps[*i].name.to_lowercase();
    match sort {
        SortKey::Name => idx.sort_by_key(name),
        SortKey::Size => idx.sort_by(|a, b| {
            let (sa, sb) = (apps[*a].size, apps[*b].size);
            sb.is_some()
                .cmp(&sa.is_some())
                .then(sb.cmp(&sa))
                .then_with(|| name(a).cmp(&name(b)))
        }),
        SortKey::InstallDate => idx.sort_by(|a, b| {
            let (da, db) = (apps[*a].install_date, apps[*b].install_date);
            db.is_some()
                .cmp(&da.is_some())
                .then(db.cmp(&da))
                .then_with(|| name(a).cmp(&name(b)))
        }),
    }
    idx
}

// MARK: - Dates

/// A date in the user's short date format, as Settings shows it ("5/13/2026").
pub fn format_date(date: NaiveDate) -> String {
    use chrono::Datelike;
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::Globalization::{GetDateFormatEx, DATE_SHORTDATE};
    let st = SYSTEMTIME {
        wYear: date.year() as u16,
        wMonth: date.month() as u16,
        wDayOfWeek: date.weekday().num_days_from_sunday() as u16,
        wDay: date.day() as u16,
        wHour: 0,
        wMinute: 0,
        wSecond: 0,
        wMilliseconds: 0,
    };
    let mut buf = [0u16; 128];
    // SAFETY: a null locale name means the user's locale; the buffer length is
    // passed in characters and the remaining pointers may be null.
    let len = unsafe {
        GetDateFormatEx(
            std::ptr::null(),
            DATE_SHORTDATE,
            &st,
            std::ptr::null(),
            buf.as_mut_ptr(),
            buf.len() as i32,
            std::ptr::null(),
        )
    };
    if len <= 1 {
        return date.format("%Y-%m-%d").to_string();
    }
    String::from_utf16_lossy(&buf[..len as usize - 1])
}

// MARK: - Uninstall

/// Why the uninstaller did not open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchError {
    /// The user declined the Windows permission prompt.
    Cancelled,
    /// The uninstaller file is missing.
    NotFound(String),
    Other(String),
}

/// Open the app's own uninstaller with `ShellExecuteEx`, so Windows can ask for
/// administrator permission when the uninstaller needs it. Returns once the
/// program has started; it then runs on its own.
pub fn launch_uninstaller(cmd: &UninstallCommand) -> Result<(), LaunchError> {
    shell::execute(cmd)
}

// MARK: - Windows APIs

mod shell {
    use super::{LaunchError, UninstallCommand};
    use std::path::Path;
    use windows_sys::Win32::Foundation::{
        GetLastError, ERROR_CANCELLED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND,
    };
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    };
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    pub fn execute(cmd: &UninstallCommand) -> Result<(), LaunchError> {
        let program = Path::new(&cmd.program);
        if program.is_absolute() && !program.is_file() {
            return Err(LaunchError::NotFound(cmd.program.clone()));
        }
        let file = wide(program.as_os_str());
        let params = wide(std::ffi::OsStr::new(&cmd.args));
        let dir = program
            .parent()
            .filter(|_| program.is_absolute())
            .map(|d| wide(d.as_os_str()));
        // SAFETY: all-zero is a valid "no options" value for this plain C struct.
        let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
        info.lpFile = file.as_ptr();
        info.lpParameters = if cmd.args.is_empty() {
            std::ptr::null()
        } else {
            params.as_ptr()
        };
        info.lpDirectory = dir.as_ref().map_or(std::ptr::null(), |d| d.as_ptr());
        info.nShow = SW_SHOWNORMAL;
        // ShellExecuteEx may hand the call to shell extensions, which need COM.
        // SAFETY: reserved pointer must be null; balanced below when it succeeded.
        let com = unsafe {
            CoInitializeEx(
                std::ptr::null(),
                (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
            )
        } >= 0;
        // SAFETY: `info` is initialised and every string it points to outlives the call.
        let ok = unsafe { ShellExecuteExW(&mut info) } != 0;
        // SAFETY: plain call, read right after the failing API.
        let err = unsafe { GetLastError() };
        if com {
            // SAFETY: balances the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
        if ok {
            return Ok(());
        }
        Err(match err {
            ERROR_CANCELLED => LaunchError::Cancelled,
            ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => {
                LaunchError::NotFound(cmd.program.clone())
            }
            code => LaunchError::Other(std::io::Error::from_raw_os_error(code as i32).to_string()),
        })
    }
}

mod reg {
    use super::{filetime_to_system, pack_guid, RegEntry, Scope};
    use std::collections::HashMap;
    use std::time::SystemTime;
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS, FILETIME,
    };
    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryInfoKeyW, RegQueryValueExW, HKEY,
        HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY,
        REG_DWORD, REG_EXPAND_SZ, REG_QWORD, REG_SAM_FLAGS, REG_SZ,
    };

    const UNINSTALL: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall";
    const MSI_USER_DATA: &str = r"Software\Microsoft\Windows\CurrentVersion\Installer\UserData";

    /// Why a key could not be read.
    pub enum ReadError {
        Missing,
        Unreadable,
    }
    pub use ReadError::{Missing, Unreadable};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn utf16(bytes: &[u8]) -> String {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .take_while(|u| *u != 0)
            .collect();
        String::from_utf16_lossy(&units)
    }

    fn expand(s: &str) -> String {
        let src = wide(s);
        // SAFETY: a null destination asks for the needed length.
        let need = unsafe { ExpandEnvironmentStringsW(src.as_ptr(), std::ptr::null_mut(), 0) };
        if need == 0 {
            return s.to_string();
        }
        let mut buf = vec![0u16; need as usize];
        // SAFETY: the buffer holds `need` characters.
        let got = unsafe { ExpandEnvironmentStringsW(src.as_ptr(), buf.as_mut_ptr(), need) };
        if got == 0 || got > need {
            return s.to_string();
        }
        String::from_utf16_lossy(&buf[..got as usize - 1])
    }

    struct Key {
        handle: HKEY,
        view: REG_SAM_FLAGS,
    }

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: the handle was opened by RegOpenKeyExW and is closed once.
            unsafe { RegCloseKey(self.handle) };
        }
    }

    impl Key {
        fn open(parent: HKEY, path: &str, view: REG_SAM_FLAGS) -> Result<Key, u32> {
            let p = wide(path);
            let mut handle: HKEY = std::ptr::null_mut();
            // SAFETY: NUL-terminated path and a valid out pointer.
            let rc = unsafe { RegOpenKeyExW(parent, p.as_ptr(), 0, KEY_READ | view, &mut handle) };
            if rc == ERROR_SUCCESS {
                Ok(Key { handle, view })
            } else {
                Err(rc)
            }
        }

        fn sub(&self, path: &str) -> Result<Key, u32> {
            Key::open(self.handle, path, self.view)
        }

        fn info(&self) -> Option<(u32, u32, FILETIME)> {
            let (mut count, mut max_len) = (0u32, 0u32);
            let mut written = FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            };
            // SAFETY: valid handle; unused outputs are null.
            let rc = unsafe {
                RegQueryInfoKeyW(
                    self.handle,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &mut count,
                    &mut max_len,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut written,
                )
            };
            (rc == ERROR_SUCCESS).then_some((count, max_len, written))
        }

        fn last_write(&self) -> Option<SystemTime> {
            let (_, _, ft) = self.info()?;
            filetime_to_system((u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime))
        }

        fn subkeys(&self) -> Vec<String> {
            let Some((count, max_len, _)) = self.info() else {
                return Vec::new();
            };
            let mut names = Vec::with_capacity(count as usize);
            let mut buf = vec![0u16; max_len as usize + 1];
            let mut index = 0u32;
            loop {
                let mut len = buf.len() as u32;
                // SAFETY: `len` is the buffer capacity in characters; the rest may be null.
                let rc = unsafe {
                    RegEnumKeyExW(
                        self.handle,
                        index,
                        buf.as_mut_ptr(),
                        &mut len,
                        std::ptr::null(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                match rc {
                    ERROR_SUCCESS => {
                        names.push(String::from_utf16_lossy(&buf[..len as usize]));
                        index += 1;
                    }
                    // A longer name was added meanwhile.
                    ERROR_MORE_DATA if buf.len() < 32_768 => buf.resize(buf.len() * 2, 0),
                    _ => break,
                }
            }
            names
        }

        fn value(&self, name: &str) -> Option<(u32, Vec<u8>)> {
            let n = wide(name);
            let (mut ty, mut size) = (0u32, 0u32);
            // SAFETY: a null data pointer asks for the type and size only.
            let rc = unsafe {
                RegQueryValueExW(
                    self.handle,
                    n.as_ptr(),
                    std::ptr::null(),
                    &mut ty,
                    std::ptr::null_mut(),
                    &mut size,
                )
            };
            if rc != ERROR_SUCCESS {
                return None;
            }
            for _ in 0..4 {
                let mut buf = vec![0u8; size as usize + 2];
                let mut len = buf.len() as u32;
                // SAFETY: `len` is the buffer size in bytes.
                let rc = unsafe {
                    RegQueryValueExW(
                        self.handle,
                        n.as_ptr(),
                        std::ptr::null(),
                        &mut ty,
                        buf.as_mut_ptr(),
                        &mut len,
                    )
                };
                match rc {
                    ERROR_SUCCESS => {
                        buf.truncate(len as usize);
                        return Some((ty, buf));
                    }
                    ERROR_MORE_DATA => size = len,
                    _ => return None,
                }
            }
            None
        }

        /// A string value, trimmed; `None` when missing or empty.
        fn text(&self, name: &str) -> Option<String> {
            let (ty, data) = self.value(name)?;
            let s = match ty {
                REG_SZ => utf16(&data),
                REG_EXPAND_SZ => expand(&utf16(&data)),
                _ => return None,
            };
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        }

        fn number(&self, name: &str) -> Option<u32> {
            let (ty, data) = self.value(name)?;
            match ty {
                REG_DWORD if data.len() >= 4 => {
                    Some(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
                }
                REG_QWORD if data.len() >= 8 => {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&data[..8]);
                    u32::try_from(u64::from_le_bytes(b)).ok()
                }
                REG_SZ => utf16(&data).trim().parse().ok(),
                _ => None,
            }
        }

        fn flag(&self, name: &str) -> bool {
            self.number(name) == Some(1)
        }
    }

    fn root(scope: Scope) -> (HKEY, REG_SAM_FLAGS) {
        match scope {
            Scope::Machine64 => (HKEY_LOCAL_MACHINE, KEY_WOW64_64KEY),
            Scope::Machine32 => (HKEY_LOCAL_MACHINE, KEY_WOW64_32KEY),
            Scope::User => (HKEY_CURRENT_USER, 0),
        }
    }

    fn open_uninstall(scope: Scope) -> Result<Key, ReadError> {
        let (hive, view) = root(scope);
        Key::open(hive, UNINSTALL, view).map_err(|rc| {
            if rc == ERROR_FILE_NOT_FOUND {
                Missing
            } else {
                Unreadable
            }
        })
    }

    /// Every subkey of one Uninstall key.
    pub fn read_uninstall(scope: Scope) -> Result<Vec<RegEntry>, ReadError> {
        let base = open_uninstall(scope)?;
        let mut out = Vec::new();
        for name in base.subkeys() {
            let Ok(k) = base.sub(&name) else { continue };
            out.push(RegEntry {
                display_name: k.text("DisplayName"),
                display_version: k.text("DisplayVersion"),
                publisher: k.text("Publisher"),
                install_location: k.text("InstallLocation"),
                estimated_size_kb: k.number("EstimatedSize"),
                install_date: k.text("InstallDate"),
                uninstall_string: k.text("UninstallString"),
                system_component: k.flag("SystemComponent"),
                parent_key_name: k.text("ParentKeyName"),
                windows_installer: k.flag("WindowsInstaller"),
                no_remove: k.flag("NoRemove"),
                last_write: k.last_write(),
                key: name,
            });
        }
        Ok(out)
    }

    pub fn key_exists(scope: Scope, key: &str) -> bool {
        match open_uninstall(scope) {
            Ok(base) => !matches!(base.sub(key), Err(ERROR_FILE_NOT_FOUND)),
            Err(Missing) => false,
            Err(Unreadable) => true,
        }
    }

    /// Size of the cached `.msi` (`LocalPackage`) of each product code, looked
    /// up for every user (`S-1-5-18` for per-machine installs). Keys are
    /// upper-cased product codes.
    pub fn msi_package_sizes(codes: &[&str]) -> HashMap<String, u64> {
        let mut out = HashMap::new();
        let Ok(user_data) = Key::open(HKEY_LOCAL_MACHINE, MSI_USER_DATA, KEY_WOW64_64KEY) else {
            return out;
        };
        let users: Vec<Key> = user_data
            .subkeys()
            .iter()
            .filter_map(|sid| user_data.sub(&format!(r"{sid}\Products")).ok())
            .collect();
        for code in codes {
            let Some(packed) = pack_guid(code) else {
                continue;
            };
            let size = users.iter().find_map(|products| {
                let props = products.sub(&format!(r"{packed}\InstallProperties")).ok()?;
                let package = props.text("LocalPackage")?;
                std::fs::metadata(package).ok().map(|m| m.len())
            });
            if let Some(size) = size {
                out.insert(code.to_ascii_uppercase(), size);
            }
        }
        out
    }

    #[cfg(test)]
    pub fn read_own_test_key() -> Option<String> {
        // HKCU\Environment always exists for a logged-on user.
        let k = Key::open(HKEY_CURRENT_USER, "Environment", 0).ok()?;
        let _ = k.subkeys();
        let _ = k.last_write()?;
        Some("ok".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("nera-apps-win-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn env() -> Env {
        Env {
            windows_dir: PathBuf::from(r"C:\Windows"),
            system32: PathBuf::from(r"C:\Windows\System32"),
            shared_roots: vec![
                PathBuf::from(r"C:\"),
                PathBuf::from(r"C:\Program Files"),
                PathBuf::from(r"C:\Program Files (x86)"),
                PathBuf::from(r"C:\Users\me\AppData\Local\Programs"),
                PathBuf::from(r"C:\Users\me"),
            ],
        }
    }

    fn entry(key: &str, name: &str) -> RegEntry {
        RegEntry {
            key: key.into(),
            display_name: Some(name.into()),
            ..Default::default()
        }
    }

    #[test]
    fn parses_install_dates_like_settings() {
        assert_eq!(
            parse_install_date("20260513"),
            NaiveDate::from_ymd_opt(2026, 5, 13)
        );
        // Discord writes this; Settings shows no date.
        assert_eq!(parse_install_date("20262820"), None);
        assert_eq!(parse_install_date("2026-05-13"), None);
        assert_eq!(parse_install_date(""), None);

        let written = SystemTime::now();
        let mut e = entry("A", "A");
        e.last_write = Some(written);
        assert_eq!(
            install_date(&e),
            Some(chrono::DateTime::<chrono::Local>::from(written).date_naive()),
            "no InstallDate: the day the entry was written"
        );
        e.install_date = Some("20262820".into());
        assert_eq!(install_date(&e), None, "an invalid date is not replaced");
    }

    #[test]
    fn sizes_like_settings() {
        assert_eq!(settings_size(None, None), None);
        assert_eq!(settings_size(Some(0), Some(5)), None);
        assert_eq!(settings_size(Some(117_760), None), Some(117_760 * 1024));
        // 7-Zip: 5947 KB plus its 2,002,432-byte cached .msi = "7.72 MB" in Settings.
        assert_eq!(settings_size(Some(5_947), Some(2_002_432)), Some(8_092_160));
    }

    #[test]
    fn splits_command_lines_like_create_process() {
        let none = |_: &Path| false;
        let c = parse_command(r#""C:\Program Files\Foo\unins000.exe" /SILENT"#, none).unwrap();
        assert_eq!(c.program, r"C:\Program Files\Foo\unins000.exe");
        assert_eq!(c.args, "/SILENT");
        let c = parse_command(r#""C:\a\Docker Desktop Installer.exe" "uninstall""#, none).unwrap();
        assert_eq!(c.args, r#""uninstall""#);
        let c = parse_command(
            r#""C:\x\RunDll32.EXE" "C:\y\NVI2.DLL",UninstallPackage Display.Driver"#,
            none,
        )
        .unwrap();
        assert_eq!(c.program, r"C:\x\RunDll32.EXE");
        assert_eq!(c.args, r#""C:\y\NVI2.DLL",UninstallPackage Display.Driver"#);
        let c =
            parse_command("MsiExec.exe /X{C4C5EF69-20FB-49FB-B9CE-715D4ED7966D}", none).unwrap();
        assert_eq!(c.program, "MsiExec.exe");
        let c = parse_command("winget uninstall --product-code X.Y", none).unwrap();
        assert_eq!(
            (c.program.as_str(), c.args.as_str()),
            ("winget", "uninstall --product-code X.Y")
        );
        assert!(parse_command("   ", none).is_none());
        assert!(parse_command(r#""""#, none).is_none());

        // Unquoted path with spaces: the existing executable wins.
        let exists = |p: &Path| p == Path::new(r"C:\Program Files\Unity\Editor\Uninstall.exe");
        let c = parse_command(r"C:\Program Files\Unity\Editor\Uninstall.exe /S", exists).unwrap();
        assert_eq!(c.program, r"C:\Program Files\Unity\Editor\Uninstall.exe");
        assert_eq!(c.args, "/S");
        // A file named like a shorter prefix but without an executable extension is never picked.
        let planted =
            |p: &Path| p == Path::new(r"C:\Program") || p == Path::new(r"C:\Program Files\X\u.exe");
        let c = parse_command(r"C:\Program Files\X\u.exe --remove", planted).unwrap();
        assert_eq!(c.program, r"C:\Program Files\X\u.exe");
        assert_eq!(c.args, "--remove");
    }

    #[test]
    fn builds_the_uninstall_command_settings_runs() {
        let env = env();
        let msiexec = r"C:\Windows\System32\msiexec.exe";
        let mut e = entry("{23170F69-40C1-2702-2601-000001000000}", "7-Zip");
        e.windows_installer = true;
        e.uninstall_string = Some("MsiExec.exe /I{23170F69-40C1-2702-2601-000001000000}".into());
        let c = uninstall_command(&e, &env, |_| false).unwrap();
        assert_eq!(c.program, msiexec);
        assert_eq!(c.args, "/x {23170F69-40C1-2702-2601-000001000000}");

        // msiexec in UninstallString without the WindowsInstaller flag: still /x.
        let mut e = entry("SomeKey", "Thing");
        e.uninstall_string = Some("MsiExec.exe /I{0E0F96AC-80DE-4400-A40C-429D63293651}".into());
        let c = uninstall_command(&e, &env, |_| false).unwrap();
        assert_eq!(
            (c.program.as_str(), c.args.as_str()),
            (msiexec, "/x {0E0F96AC-80DE-4400-A40C-429D63293651}")
        );

        // Bare system tools resolve to System32 when present there.
        let mut e = entry("K", "K");
        e.uninstall_string = Some("rundll32.exe foo.dll,Uninstall".into());
        let c = uninstall_command(&e, &env, |p| {
            p == Path::new(r"C:\Windows\System32\rundll32.exe")
        })
        .unwrap();
        assert_eq!(c.program, r"C:\Windows\System32\rundll32.exe");
        let mut e = entry("K", "K");
        e.uninstall_string = Some("winget uninstall --product-code X".into());
        assert_eq!(
            uninstall_command(&e, &env, |_| false).unwrap().program,
            "winget"
        );

        assert!(uninstall_command(&entry("K", "K"), &env, |_| false).is_none());
        assert_eq!(
            UninstallCommand {
                program: r"C:\Program Files\A\u.exe".into(),
                args: "/S".into()
            }
            .display(),
            r#""C:\Program Files\A\u.exe" /S"#
        );
    }

    #[test]
    fn product_codes_and_packed_guids() {
        assert!(msi_product_code("{23170F69-40C1-2702-2601-000001000000}").is_some());
        assert!(msi_product_code("{23170F69-40C1-2702-2601-000001000000}_is1").is_none());
        assert!(msi_product_code("Git_is1").is_none());
        assert_eq!(
            guid_in("/I{0E0F96AC-80DE-4400-A40C-429D63293651} /q"),
            Some("{0E0F96AC-80DE-4400-A40C-429D63293651}")
        );
        // Windows Installer's packed form: groups reversed, then byte pairs swapped.
        assert_eq!(
            pack_guid("{12345678-ABCD-EF01-2345-6789ABCDEF01}").as_deref(),
            Some("87654321DCBA10FE32547698BADCFE10")
        );
        assert!(pack_guid("nope").is_none());
    }

    #[test]
    fn locations_and_own_folders() {
        let env = env();
        assert_eq!(
            clean_location(r#""C:\Program Files (x86)\Microsoft Visual Studio\Installer""#),
            Some(PathBuf::from(
                r"C:\Program Files (x86)\Microsoft Visual Studio\Installer"
            ))
        );
        assert_eq!(
            clean_location(r"C:\Program Files\Git\"),
            Some(PathBuf::from(r"C:\Program Files\Git"))
        );
        assert_eq!(clean_location(r"C:\"), Some(PathBuf::from(r"C:\")));
        assert_eq!(clean_location("relative\\dir"), None);
        assert_eq!(clean_location("  "), None);

        assert!(is_own_folder(Path::new(r"C:\Program Files\Git"), &env));
        assert!(is_own_folder(Path::new(r"C:\BuildTools"), &env));
        assert!(!is_own_folder(Path::new(r"C:\"), &env));
        assert!(!is_own_folder(Path::new(r"C:\Program Files"), &env));
        assert!(!is_own_folder(Path::new(r"c:\program files\"), &env));
        assert!(!is_own_folder(Path::new(r"C:\Users\me"), &env));
        assert!(!is_own_folder(
            Path::new(r"C:\Windows\Installer\Razer"),
            &env
        ));
        assert!(!is_own_folder(
            Path::new(r"C:\ProgramData\Package Cache\{x}"),
            &env
        ));
    }

    #[test]
    fn lists_entries_like_settings() {
        let tmp = TempDir::new("entries");
        let app_dir = tmp.0.join("Foo");
        std::fs::create_dir_all(&app_dir).unwrap();
        let uninstaller = app_dir.join("unins000.exe");
        std::fs::write(&uninstaller, b"MZ").unwrap();
        let env = env();
        let me = SelfIdentity { executable: None };

        let mut e = entry("Foo_is1", "Foo");
        e.display_version = Some("1.2".into());
        e.publisher = Some("Foo Inc.".into());
        e.estimated_size_kb = Some(2048);
        e.install_date = Some("20260102".into());
        e.uninstall_string = Some(format!("\"{}\" /x", uninstaller.display()));
        let app = app_from_entry(Scope::User, &e, Some(999), &env, &me).unwrap();
        assert_eq!(app.id, "HKCU\\Foo_is1");
        assert_eq!(
            (
                app.name.as_str(),
                app.version.as_str(),
                app.publisher.as_str()
            ),
            ("Foo", "1.2", "Foo Inc.")
        );
        assert_eq!(
            app.size,
            Some(2048 * 1024),
            "the .msi only counts for Windows Installer entries"
        );
        assert_eq!(app.install_date, NaiveDate::from_ymd_opt(2026, 1, 2));
        assert_eq!(app.install_location, None);
        assert_eq!(
            app.app_folder.as_deref(),
            Some(app_dir.as_path()),
            "the uninstaller's folder"
        );
        assert!(app.can_uninstall());

        e.install_location = Some(format!("{}\\", app_dir.display()));
        let app = app_from_entry(Scope::User, &e, None, &env, &me).unwrap();
        assert_eq!(app.install_location.as_deref(), Some(app_dir.as_path()));

        // Nera itself.
        let me_inside = SelfIdentity {
            executable: Some(app_dir.join("nera-cleaner.exe")),
        };
        let app = app_from_entry(Scope::User, &e, None, &env, &me_inside).unwrap();
        assert_eq!(app.protection, Protection::SelfApp);
        assert!(!app.can_uninstall());

        // NoRemove and missing uninstall commands are listed without the action.
        let mut edge = entry("Edge", "Microsoft Edge");
        edge.no_remove = true;
        edge.uninstall_string = Some(format!("\"{}\"", uninstaller.display()));
        let edge = app_from_entry(Scope::Machine32, &edge, None, &env, &me).unwrap();
        assert!(!edge.can_uninstall());
        let toolbox = app_from_entry(
            Scope::User,
            &entry("JetBrains Toolbox (Rider)", "Rider"),
            None,
            &env,
            &me,
        )
        .unwrap();
        assert!(toolbox.uninstall.is_none() && !toolbox.can_uninstall());

        // Hidden, as in Settings.
        let mut sys = entry("S", "Hidden");
        sys.system_component = true;
        assert!(app_from_entry(Scope::Machine64, &sys, None, &env, &me).is_none());
        let mut update = entry("U", "Update for Foo");
        update.parent_key_name = Some("Foo".into());
        assert!(app_from_entry(Scope::Machine64, &update, None, &env, &me).is_none());
        let mut unnamed = entry("N", "");
        unnamed.display_name = None;
        assert!(app_from_entry(Scope::Machine64, &unnamed, None, &env, &me).is_none());

        // Windows Installer: Settings adds the cached package to the size.
        let mut msi = entry("{23170F69-40C1-2702-2601-000001000000}", "7-Zip");
        msi.windows_installer = true;
        msi.estimated_size_kb = Some(5_947);
        let app = app_from_entry(Scope::Machine64, &msi, Some(2_002_432), &env, &me).unwrap();
        assert_eq!(app.size, Some(8_092_160));
        assert!(
            app.app_folder.is_none(),
            "msiexec's folder is never the app's"
        );
    }

    #[test]
    fn running_means_a_process_inside_the_app_folder() {
        let mut app = app_from_entry(
            Scope::User,
            &entry("K", "K"),
            None,
            &env(),
            &SelfIdentity::default(),
        )
        .unwrap();
        let set = RunningSet::from_paths(vec![PathBuf::from(r"C:\Program Files\Foo\bin\foo.exe")]);
        assert!(!set.is_app_running(&app), "no folder, no claim");
        app.app_folder = Some(PathBuf::from(r"c:\program files\foo"));
        assert!(set.is_app_running(&app));
        app.app_folder = Some(PathBuf::from(r"C:\Program Files\Foo Bar"));
        assert!(!set.is_app_running(&app));
    }

    #[test]
    fn filters_and_sorts_like_settings() {
        let env = env();
        let me = SelfIdentity::default();
        let mk = |name: &str, publisher: &str, kb: Option<u32>, date: Option<&str>| {
            let mut e = entry(name, name);
            e.publisher = Some(publisher.into());
            e.estimated_size_kb = kb;
            e.install_date = date.map(Into::into);
            app_from_entry(Scope::User, &e, None, &env, &me).unwrap()
        };
        let apps = vec![
            mk("zed", "Zed Industries", Some(10), Some("20260101")),
            mk("Arc", "The Browser Company", Some(50), Some("20250101")),
            mk("Mail", "Example", None, Some("20262820")),
        ];
        assert_eq!(filter_sort(&apps, "", SortKey::Name), vec![1, 2, 0]);
        assert_eq!(filter_sort(&apps, "", SortKey::Size), vec![1, 0, 2]);
        assert_eq!(filter_sort(&apps, "", SortKey::InstallDate), vec![0, 1, 2]);
        assert_eq!(filter_sort(&apps, "ZED", SortKey::Name), vec![0]);
        assert_eq!(filter_sort(&apps, "browser", SortKey::Name), vec![1]);
        assert!(filter_sort(&apps, "nothing", SortKey::Name).is_empty());
    }

    #[test]
    fn converts_filetimes() {
        // 2026-01-01T00:00:00Z
        let t = filetime_to_system(134_116_992_000_000_000).unwrap();
        assert_eq!(
            t.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_767_225_600
        );
        assert!(filetime_to_system(0).is_none());
    }

    #[test]
    fn formats_dates_in_the_users_format() {
        let s = format_date(NaiveDate::from_ymd_opt(2026, 5, 13).unwrap());
        assert!(s.contains("2026") || s.contains("26"), "{s}");
        assert!(s.contains('5') && s.contains("13"), "{s}");
    }

    #[test]
    fn a_missing_uninstaller_is_reported_without_launching() {
        let tmp = TempDir::new("launch");
        let cmd = UninstallCommand {
            program: tmp.0.join("gone.exe").display().to_string(),
            args: "/S".into(),
        };
        assert_eq!(
            launch_uninstaller(&cmd),
            Err(LaunchError::NotFound(cmd.program.clone()))
        );
    }

    #[test]
    fn reads_the_real_registry_read_only() {
        assert!(reg::read_own_test_key().is_some());
        let found = discover(&SelfIdentity::current(), &RunningSet::default());
        for app in &found.apps {
            assert!(!app.name.is_empty());
            assert!(app.id.starts_with(app.scope.tag()));
            assert!(is_still_listed(app.scope, &app.key), "{}", app.id);
        }
        assert!(!is_still_listed(
            Scope::User,
            "nera-cleaner-test-no-such-key"
        ));
    }
}
