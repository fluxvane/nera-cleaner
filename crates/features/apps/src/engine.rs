//! App uninstaller engine: discovers installed apps, measures them, finds
//! their leftovers in `~/Library`, and moves an app plus selected leftovers
//! to the Trash. Port of `ScannerActor.discoverApps/findLeftovers`,
//! `InstalledApp` and `UninstallAppUseCase`.
//!
//! Nothing here touches the UI. Every removal goes through the `trash`
//! callback the caller passes (in the app: `nera_core::fs_util::trash_item`).

use nera_core::fs_util::{self, Cancel};
use nera_core::paths;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// MARK: - Model

/// Why an app may or may not be uninstalled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protection {
    Removable,
    /// Part of macOS or made by Apple (`/System/Applications`, `com.apple.*`).
    Apple,
    /// Nera Cleaner itself.
    SelfApp,
}

/// One installed application bundle. `path` is its stable id.
#[derive(Clone, Debug)]
pub struct InstalledApp {
    pub path: PathBuf,
    /// `path` with links resolved (Safari lives behind a link into the system volume).
    pub real_path: PathBuf,
    /// `CFBundleExecutable`: the file under `Contents/MacOS` that runs when the app is open.
    pub executable: Option<String>,
    /// Name as Finder shows it (bundle file name without `.app`).
    pub name: String,
    pub bundle_id: String,
    pub version: String,
    /// `CFBundleName` / `CFBundleDisplayName` when they differ from `name`;
    /// used to match leftover folders such as `Application Support/Code`.
    pub alt_names: Vec<String>,
    /// The bundle's `.icns` file, if it has one.
    pub icon_file: Option<PathBuf>,
    pub protection: Protection,
    pub running: bool,
    /// Allocated size of the bundle; `None` until measured.
    pub size: Option<u64>,
    pub last_used: Option<SystemTime>,
    /// PNG rendition of the icon in Nera's cache; `None` until converted.
    pub icon_png: Option<PathBuf>,
}

impl InstalledApp {
    pub fn is_removable(&self) -> bool {
        self.protection == Protection::Removable
    }

    /// Lower-cased names that identify this app's leftover folders.
    pub fn match_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for n in std::iter::once(&self.name).chain(self.alt_names.iter()) {
            let n = n.trim().to_lowercase();
            if is_specific_name(&n) && !names.contains(&n) {
                names.push(n);
            }
        }
        names
    }
}

/// A folder that holds applications.
#[derive(Clone, Debug)]
pub struct AppRoot {
    pub path: PathBuf,
    /// Apps here belong to macOS and are never removable.
    pub system: bool,
}

/// `/Applications`, `~/Applications` and `/System/Applications`.
pub fn default_roots() -> Vec<AppRoot> {
    vec![
        AppRoot {
            path: PathBuf::from("/Applications"),
            system: false,
        },
        AppRoot {
            path: paths::home_join("Applications"),
            system: false,
        },
        AppRoot {
            path: PathBuf::from("/System/Applications"),
            system: true,
        },
    ]
}

/// What identifies "this app" so it can never uninstall itself.
#[derive(Clone, Debug, Default)]
pub struct SelfIdentity {
    pub bundle_id: String,
    pub executable: Option<PathBuf>,
}

impl SelfIdentity {
    pub fn current() -> Self {
        Self {
            bundle_id: paths::BUNDLE_ID.to_string(),
            executable: std::env::current_exe()
                .ok()
                .and_then(|p| p.canonicalize().ok().or(Some(p))),
        }
    }

    fn is_self(&self, app_path: &Path, bundle_id: &str) -> bool {
        if !bundle_id.is_empty() && bundle_id.eq_ignore_ascii_case(&self.bundle_id) {
            return true;
        }
        let real = app_path
            .canonicalize()
            .unwrap_or_else(|_| app_path.to_path_buf());
        self.executable
            .as_ref()
            .is_some_and(|exe| exe.starts_with(&real))
    }
}

/// Executable paths of running processes, captured once.
#[derive(Clone, Debug, Default)]
pub struct RunningSet {
    exes: HashSet<PathBuf>,
}

impl RunningSet {
    /// Every running process, by the executable file it runs and by the path it
    /// was started from. The two differ for apps that run from a copy: Chrome
    /// starts from a clone under `/private/var/folders/...` while the process
    /// was launched as `/Applications/Google Chrome.app/Contents/MacOS/...`.
    pub fn snapshot() -> Self {
        let mut exes = HashSet::new();
        for pid in process_ids() {
            exes.extend(fs_util::executable_path(pid));
            exes.extend(launch_path(pid));
        }
        Self { exes }
    }

    pub fn from_paths(exes: Vec<PathBuf>) -> Self {
        Self {
            exes: exes.into_iter().collect(),
        }
    }

    /// True when the app itself is open: a process runs the bundle's main
    /// executable (`CFBundleExecutable`), as when LaunchServices lists the app
    /// as running. Helper tools, widgets and extensions that live inside the
    /// bundle (Xcode's `make`, a Mail extension) do not make an app "open", and
    /// neither does a bundle whose executable is only a link to some other
    /// program that happens to run. Reads the bundle's Info.plist: call it off
    /// the UI thread, or use [`RunningSet::is_app_running`].
    pub fn is_running(&self, app: &Path) -> bool {
        let (real, exe) = bundle_identity(app);
        self.matches(app, &real, exe.as_deref())
    }

    /// Like [`RunningSet::is_running`] for a discovered app, without touching the disk.
    pub fn is_app_running(&self, app: &InstalledApp) -> bool {
        self.matches(&app.path, &app.real_path, app.executable.as_deref())
    }

    fn matches(&self, nominal: &Path, real: &Path, executable: Option<&str>) -> bool {
        let dirs = [real.join("Contents/MacOS"), nominal.join("Contents/MacOS")];
        match executable {
            // Compare the executable path as the process reports it: the bundle
            // folder may be resolved, the executable file itself is not.
            Some(name) => dirs.iter().any(|d| self.exes.contains(&d.join(name))),
            // No readable Info.plist: anything running from Contents/MacOS.
            None => self
                .exes
                .iter()
                .any(|e| dirs.iter().any(|d| e.starts_with(d))),
        }
    }
}

/// PIDs of all processes. Retries with a bigger buffer when processes started
/// between sizing the list and reading it, so none is missed.
fn process_ids() -> Vec<i32> {
    // SAFETY: a null buffer asks for the count only.
    let mut capacity =
        unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) }.max(0) as usize + 256;
    loop {
        let mut pids = vec![0i32; capacity];
        let bytes = (pids.len() * std::mem::size_of::<i32>()) as i32;
        // SAFETY: the buffer is valid for `bytes` bytes.
        let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if got <= 0 {
            return Vec::new();
        }
        if (got as usize) < capacity {
            pids.truncate(got as usize);
            pids.retain(|p| *p > 0);
            return pids;
        }
        capacity *= 2;
    }
}

/// The path a process was started with (`KERN_PROCARGS2`: the `exec` path), for
/// processes of this user; `None` when macOS does not allow reading it.
fn launch_path(pid: i32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: usize = 0;
    // SAFETY: a null buffer asks for the size only.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size <= 4 {
        return None;
    }
    let mut buf = vec![0u8; size];
    // SAFETY: the buffer is valid for `size` bytes.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size <= 4 {
        return None;
    }
    // Layout: argc (i32), then the NUL-terminated exec path, then the arguments.
    let rest = &buf[4..size];
    let end = rest.iter().position(|b| *b == 0)?;
    let path = Path::new(std::ffi::OsStr::from_bytes(&rest[..end]));
    path.is_absolute().then(|| path.to_path_buf())
}

/// Resolved bundle folder and `CFBundleExecutable` of an app bundle.
fn bundle_identity(app: &Path) -> (PathBuf, Option<String>) {
    let real = app.canonicalize().unwrap_or_else(|_| app.to_path_buf());
    let exe = plist::Value::from_file(app.join("Contents/Info.plist"))
        .ok()
        .and_then(|p| {
            p.as_dictionary()?
                .get("CFBundleExecutable")?
                .as_string()
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty());
    (real, exe)
}

/// Result of [`discover`].
#[derive(Debug, Default)]
pub struct Discovery {
    pub apps: Vec<InstalledApp>,
    /// App folders that exist but could not be listed.
    pub unreadable: Vec<PathBuf>,
}

// MARK: - Discovery

/// List `.app` bundles in each root and one level of subfolders
/// (e.g. `/Applications/Utilities`). Symlinked bundles are skipped (removing
/// a link is not an uninstall). Sorted by name; sizes are measured later.
pub fn discover(roots: &[AppRoot], me: &SelfIdentity, running: &RunningSet) -> Discovery {
    let mut out = Discovery::default();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for root in roots {
        let entries = match std::fs::read_dir(&root.path) {
            Ok(e) => e,
            Err(err) => {
                if root.path.exists() && err.kind() == io::ErrorKind::PermissionDenied {
                    out.unreadable.push(root.path.clone());
                }
                continue;
            }
        };
        let mut subfolders = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if is_hidden(&path) || !(ft.is_dir() || is_system_bundle_link(&path, &ft)) {
                continue;
            }
            if is_app_bundle(&path) {
                push_app(&path, root, me, running, &mut seen, &mut out.apps);
            } else {
                subfolders.push(path);
            }
        }
        for folder in subfolders {
            let Ok(entries) = std::fs::read_dir(&folder) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(ft) = entry.file_type() else { continue };
                if !is_hidden(&path)
                    && is_app_bundle(&path)
                    && (ft.is_dir() || is_system_bundle_link(&path, &ft))
                {
                    push_app(&path, root, me, running, &mut seen, &mut out.apps);
                }
            }
        }
    }
    out.apps.sort_by_key(|a| a.name.to_lowercase());
    out
}

fn push_app(
    path: &Path,
    root: &AppRoot,
    me: &SelfIdentity,
    running: &RunningSet,
    seen: &mut HashSet<PathBuf>,
    apps: &mut Vec<InstalledApp>,
) {
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !seen.insert(key) {
        return;
    }
    if let Some(mut app) = read_app(path, root.system, me) {
        app.running = running.is_app_running(&app);
        apps.push(app);
    }
}

/// macOS exposes some of its own apps (Safari) in `/Applications` as links
/// into the sealed system volume. They are real, listed apps; links to
/// anywhere else are not (removing a link is not an uninstall).
fn is_system_bundle_link(path: &Path, ft: &std::fs::FileType) -> bool {
    ft.is_symlink()
        && is_app_bundle(path)
        && path
            .canonicalize()
            .is_ok_and(|real| real.is_dir() && real.starts_with("/System"))
}

fn is_app_bundle(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("app"))
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
}

/// Read one bundle's `Info.plist`. Returns `None` for bundles without one.
pub fn read_app(path: &Path, system_root: bool, me: &SelfIdentity) -> Option<InstalledApp> {
    let plist = plist::Value::from_file(path.join("Contents/Info.plist")).ok()?;
    let dict = plist.as_dictionary()?;
    let get = |k: &str| {
        dict.get(k)
            .and_then(|v| v.as_string())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let executable = get("CFBundleExecutable");
    let bundle_id = get("CFBundleIdentifier").unwrap_or_default();
    let version = get("CFBundleShortVersionString")
        .or_else(|| get("CFBundleVersion"))
        .unwrap_or_default();
    let mut alt_names = Vec::new();
    for key in ["CFBundleName", "CFBundleDisplayName"] {
        if let Some(n) = get(key) {
            if !n.eq_ignore_ascii_case(&name)
                && !alt_names
                    .iter()
                    .any(|a: &String| a.eq_ignore_ascii_case(&n))
            {
                alt_names.push(n);
            }
        }
    }
    let icon_file = resolve_icon_file(path, get("CFBundleIconFile").as_deref());
    let protection = protection_for(path, &bundle_id, system_root, me);

    Some(InstalledApp {
        path: path.to_path_buf(),
        real_path: path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
        executable,
        name,
        bundle_id,
        version,
        alt_names,
        icon_file,
        protection,
        running: false,
        size: None,
        last_used: None,
        icon_png: None,
    })
}

/// Apple apps and Nera itself are never removable.
pub fn protection_for(
    path: &Path,
    bundle_id: &str,
    system_root: bool,
    me: &SelfIdentity,
) -> Protection {
    if me.is_self(path, bundle_id) {
        return Protection::SelfApp;
    }
    let apple_id = bundle_id.to_lowercase().starts_with("com.apple.");
    if system_root || apple_id || paths::is_under(path, "/System") {
        return Protection::Apple;
    }
    Protection::Removable
}

fn resolve_icon_file(app: &Path, icon_file: Option<&str>) -> Option<PathBuf> {
    let resources = app.join("Contents/Resources");
    if let Some(name) = icon_file {
        let mut p = resources.join(name);
        if p.extension().is_none() {
            p.set_extension("icns");
        }
        if p.is_file() {
            return Some(p);
        }
    }
    let fallback = resources.join("AppIcon.icns");
    fallback.is_file().then_some(fallback)
}

// MARK: - Measuring

/// Recursive allocated (on-disk) size of an app bundle, as Finder's "size on disk".
pub fn app_size(app: &Path, cancel: &Cancel) -> u64 {
    // Safari is listed through a link into the system volume; measure the bundle.
    tree_size(
        &app.canonicalize().unwrap_or_else(|_| app.to_path_buf()),
        cancel,
    )
}

/// `UF_COMPRESSED`: the file's data is stored compressed (APFS/HFS+ transparent compression).
const UF_COMPRESSED: u32 = 0x20;

/// Space `meta` takes on disk, as Finder and Spotlight report it
/// (`ATTR_FILE_ALLOCSIZE`). That equals `st_blocks * 512` except for
/// transparently compressed files whose small payload sits inside the file's
/// metadata: `st_blocks` still says one 4 KB block, the file system allocates
/// none. `path` is only used for those files.
pub fn allocated(path: impl FnOnce() -> PathBuf, meta: &std::fs::Metadata) -> u64 {
    use std::os::macos::fs::MetadataExt;
    let blocks = fs_util::allocated_size(meta);
    if blocks == 0 || !meta.is_file() || meta.st_flags() & UF_COMPRESSED == 0 {
        return blocks;
    }
    attr_alloc_size(&path()).unwrap_or(blocks)
}

/// `ATTR_FILE_ALLOCSIZE` of a file via `getattrlist`; `None` when unavailable.
fn attr_alloc_size(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: a zeroed attrlist is a valid "no attributes" request; fields set below.
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

/// Space `path` takes on disk: allocated blocks of every file and folder under
/// it (hidden files included, symlinks not followed). A file with several
/// hard links is counted once, as `du` and Spotlight do (Xcode and macOS apps
/// share many files that way). Returns a partial total when cancelled.
pub fn tree_size(path: &Path, cancel: &Cancel) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return allocated(|| path.to_path_buf(), &meta);
    }
    let mut total = fs_util::allocated_size(&meta);
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut stack = vec![path.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited.is_multiple_of(512) && cancel.is_cancelled() {
                return total;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                total += fs_util::allocated_size(&meta);
                stack.push(entry.path());
            } else if meta.nlink() <= 1 || seen.insert((meta.dev(), meta.ino())) {
                total += allocated(|| entry.path(), &meta);
            }
        }
    }
    total
}

/// Where converted app icons are cached.
pub fn icon_cache_dir() -> PathBuf {
    paths::home_join("Library/Caches")
        .join(paths::BUNDLE_ID)
        .join("icons")
}

/// Convert an `.icns` file to a 64 px PNG in `cache_dir` (via `sips`), reusing
/// an earlier conversion when the source has not changed.
pub fn icon_png(icns: &Path, cache_dir: &Path) -> Option<PathBuf> {
    let meta = std::fs::metadata(icns).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    icns.hash(&mut hasher);
    meta.len().hash(&mut hasher);
    meta.modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .hash(&mut hasher);
    let out = cache_dir.join(format!("{:016x}.png", hasher.finish()));
    if out.is_file() {
        return Some(out);
    }
    std::fs::create_dir_all(cache_dir).ok()?;
    let tmp = cache_dir.join(format!(
        ".{:016x}.{}.tmp.png",
        hasher.finish(),
        std::process::id()
    ));
    let status = Command::new("/usr/bin/sips")
        .args(["-s", "format", "png", "-Z", "64"])
        .arg(icns)
        .arg("--out")
        .arg(&tmp)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    if !status.success() || !tmp.is_file() {
        return None;
    }
    std::fs::rename(&tmp, &out).ok()?;
    Some(out)
}

/// `kMDItemLastUsedDate` for each bundle (Spotlight), in input order.
/// Missing or unparsable dates are `None`; failures yield all `None`.
pub fn last_used_dates(apps: &[PathBuf]) -> Vec<Option<SystemTime>> {
    let mut out = Vec::with_capacity(apps.len());
    for chunk in apps.chunks(64) {
        let parsed = Command::new("/usr/bin/mdls")
            .args(["-name", "kMDItemLastUsedDate", "-raw"])
            .args(chunk)
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .map(|o| parse_mdls_raw(&String::from_utf8_lossy(&o.stdout)))
            .filter(|v| v.len() == chunk.len());
        match parsed {
            Some(values) => out.extend(values),
            None => out.extend(std::iter::repeat_n(None, chunk.len())),
        }
    }
    out
}

/// Split `mdls -raw` output (NUL-separated values) into dates.
pub fn parse_mdls_raw(stdout: &str) -> Vec<Option<SystemTime>> {
    if stdout.is_empty() {
        return Vec::new();
    }
    stdout
        .trim_end_matches('\n')
        .split('\0')
        .map(parse_mdls_date)
        .collect()
}

/// Parse `2026-11-09 16:29:23 +0000`.
pub fn parse_mdls_date(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    let mut parts = s.split_whitespace();
    let date = parts.next()?;
    let time = parts.next()?;
    let offset = parts.next().unwrap_or("+0000");
    let mut d = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.split(':').map(|x| x.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || offset.len() != 5 {
        return None;
    }
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    let off_h: i64 = offset.get(1..3)?.parse().ok()?;
    let off_m: i64 = offset.get(3..5)?.parse().ok()?;
    let secs = days_from_civil(y, m, day) * 86_400 + hh * 3600 + mm * 60 + ss
        - sign * (off_h * 3600 + off_m * 60);
    (secs >= 0).then(|| UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Local calendar date of `t`.
fn local_date(t: SystemTime) -> chrono::NaiveDate {
    chrono::DateTime::<chrono::Local>::from(t).date_naive()
}

/// Whole calendar months from `from` to `to` (0 when `to` is not later).
fn whole_months(from: chrono::NaiveDate, to: chrono::NaiveDate) -> u32 {
    use chrono::Datelike;
    let mut months = (to.year() - from.year()) * 12 + to.month() as i32 - from.month() as i32;
    if to.day() < from.day() {
        months -= 1;
    }
    months.max(0) as u32
}

/// The moment an app was last in use: right now while it runs (Spotlight only
/// records launches, so a long-running app would look "unused" otherwise).
pub fn effective_last_used(app: &InstalledApp, now: SystemTime) -> Option<SystemTime> {
    if app.running {
        Some(now)
    } else {
        app.last_used
    }
}

/// "Used today", "Used 3 days ago", "Used 2 months ago", "No usage data".
/// Days and months are calendar days and months in the local time zone.
pub fn describe_last_used(t: Option<SystemTime>, now: SystemTime) -> String {
    let Some(t) = t else {
        return "No usage data".into();
    };
    let (then, today) = (local_date(t), local_date(now));
    let months = whole_months(then, today);
    if months == 0 {
        let days = (today - then).num_days().max(0);
        return match days {
            0 => "Used today".into(),
            1 => "Used yesterday".into(),
            n => format!("Used {n} days ago"),
        };
    }
    if months < 12 {
        return if months == 1 {
            "Used a month ago".into()
        } else {
            format!("Used {months} months ago")
        };
    }
    let years = months / 12;
    if years == 1 {
        "Used a year ago".into()
    } else {
        format!("Used {years} years ago")
    }
}

/// Not opened for at least `months` calendar months (apps without usage data
/// do not count; a running app counts as in use).
pub fn unused_for(app: &InstalledApp, months: u32, now: SystemTime) -> bool {
    effective_last_used(app, now)
        .is_some_and(|t| whole_months(local_date(t), local_date(now)) >= months)
}

// MARK: - Filtering and sorting

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SortKey {
    #[default]
    Name,
    /// Largest first; unmeasured last.
    Size,
    /// Least recently used first; apps without usage data last.
    LastUsed,
}

/// Indices of the apps matching `query` (name or bundle id, case-insensitive),
/// optionally hiding Apple apps, in `sort` order.
pub fn filter_sort(
    apps: &[InstalledApp],
    query: &str,
    sort: SortKey,
    show_apple: bool,
    now: SystemTime,
) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    let mut idx: Vec<usize> = apps
        .iter()
        .enumerate()
        .filter(|(_, a)| show_apple || a.protection != Protection::Apple)
        .filter(|(_, a)| {
            q.is_empty()
                || a.name.to_lowercase().contains(&q)
                || a.bundle_id.to_lowercase().contains(&q)
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
        SortKey::LastUsed => idx.sort_by(|a, b| {
            let (ta, tb) = (
                effective_last_used(&apps[*a], now),
                effective_last_used(&apps[*b], now),
            );
            ta.is_none()
                .cmp(&tb.is_none())
                .then(ta.cmp(&tb))
                .then_with(|| name(a).cmp(&name(b)))
        }),
    }
    idx
}

// MARK: - Leftovers

/// Library folder a leftover lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LeftoverKind {
    ApplicationSupport,
    ApplicationScripts,
    Caches,
    Preferences,
    Containers,
    GroupContainers,
    Logs,
    SavedState,
    HttpStorages,
    WebKit,
    Cookies,
    LaunchAgents,
}

impl LeftoverKind {
    pub const ALL: [LeftoverKind; 12] = [
        LeftoverKind::ApplicationSupport,
        LeftoverKind::ApplicationScripts,
        LeftoverKind::Caches,
        LeftoverKind::Preferences,
        LeftoverKind::Containers,
        LeftoverKind::GroupContainers,
        LeftoverKind::Logs,
        LeftoverKind::SavedState,
        LeftoverKind::HttpStorages,
        LeftoverKind::WebKit,
        LeftoverKind::Cookies,
        LeftoverKind::LaunchAgents,
    ];

    /// Folders (relative to `~/Library`) searched for this kind.
    pub fn folders(self) -> &'static [&'static str] {
        match self {
            LeftoverKind::ApplicationSupport => &["Application Support"],
            LeftoverKind::ApplicationScripts => &["Application Scripts"],
            LeftoverKind::Caches => &["Caches"],
            LeftoverKind::Preferences => &["Preferences", "Preferences/ByHost"],
            LeftoverKind::Containers => &["Containers"],
            LeftoverKind::GroupContainers => &["Group Containers"],
            LeftoverKind::Logs => &["Logs"],
            LeftoverKind::SavedState => &["Saved Application State"],
            LeftoverKind::HttpStorages => &["HTTPStorages"],
            LeftoverKind::WebKit => &["WebKit"],
            LeftoverKind::Cookies => &["Cookies"],
            LeftoverKind::LaunchAgents => &["LaunchAgents"],
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LeftoverKind::ApplicationSupport => "Application support",
            LeftoverKind::ApplicationScripts => "Application scripts",
            LeftoverKind::Caches => "Cache",
            LeftoverKind::Preferences => "Preferences",
            LeftoverKind::Containers => "Container",
            LeftoverKind::GroupContainers => "Group container",
            LeftoverKind::Logs => "Logs",
            LeftoverKind::SavedState => "Saved state",
            LeftoverKind::HttpStorages => "Web storage",
            LeftoverKind::WebKit => "WebKit data",
            LeftoverKind::Cookies => "Cookies",
            LeftoverKind::LaunchAgents => "Launch agent",
        }
    }

    /// Lucide icon name.
    pub fn icon(self) -> &'static str {
        match self {
            LeftoverKind::ApplicationSupport => "folder-cog",
            LeftoverKind::ApplicationScripts => "file-cog",
            LeftoverKind::Caches => "database",
            LeftoverKind::Preferences => "settings",
            LeftoverKind::Containers => "box",
            LeftoverKind::GroupContainers => "boxes",
            LeftoverKind::Logs => "scroll-text",
            LeftoverKind::SavedState => "save",
            LeftoverKind::HttpStorages => "globe",
            LeftoverKind::WebKit => "globe",
            LeftoverKind::Cookies => "cookie",
            LeftoverKind::LaunchAgents => "rocket",
        }
    }
}

/// A file or folder an app left in `~/Library`. `path` is its stable id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leftover {
    pub path: PathBuf,
    pub kind: LeftoverKind,
    pub size: u64,
}

/// What to look for: the app, plus every other installed app so data they
/// share (or that matches them more precisely) is never claimed.
#[derive(Clone, Debug, Default)]
pub struct LeftoverQuery {
    pub bundle_id: String,
    /// Lower-cased, see [`InstalledApp::match_names`].
    pub names: Vec<String>,
    /// `(bundle_id, names)` of the other installed apps.
    pub others: Vec<(String, Vec<String>)>,
}

impl LeftoverQuery {
    pub fn for_app(app: &InstalledApp, all: &[InstalledApp]) -> Self {
        Self {
            bundle_id: app.bundle_id.clone(),
            names: app.match_names(),
            others: all
                .iter()
                .filter(|o| o.path != app.path)
                .map(|o| (o.bundle_id.clone(), o.match_names()))
                .collect(),
        }
    }
}

#[derive(Debug, Default)]
pub struct LeftoverScan {
    pub items: Vec<Leftover>,
    /// Library folders that could not be read (Full Disk Access missing).
    pub denied: Vec<PathBuf>,
    /// Matching items kept because another installed app uses them too.
    pub shared_skipped: usize,
}

/// Search `library` (normally `~/Library`) for leftovers of one app.
/// Only top-level entries of each folder are considered, matched exactly by
/// bundle id (or `<bundle id>.*`) or by app name — never by substring.
pub fn find_leftovers(library: &Path, query: &LeftoverQuery, cancel: &Cancel) -> LeftoverScan {
    let mut scan = LeftoverScan::default();
    let bundle = query.bundle_id.trim().to_lowercase();
    let mut seen = HashSet::new();
    for kind in LeftoverKind::ALL {
        for folder in kind.folders() {
            if cancel.is_cancelled() {
                return scan;
            }
            let dir = library.join(folder);
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(err) => {
                    if err.kind() == io::ErrorKind::PermissionDenied {
                        scan.denied.push(dir);
                    }
                    continue;
                }
            };
            // Team-prefixed names (`TEAMID.bundle.id`) are used by these two folders.
            let group = matches!(
                kind,
                LeftoverKind::GroupContainers | LeftoverKind::ApplicationScripts
            );
            for entry in entries.flatten() {
                let path = entry.path();
                let item = entry.file_name().to_string_lossy().to_lowercase();
                if item.starts_with('.') || (*folder == "Preferences" && item == "byhost") {
                    continue;
                }
                let ours = match_score(&item, &bundle, &query.names, group);
                if ours == 0 {
                    continue;
                }
                let theirs = query
                    .others
                    .iter()
                    .map(|(b, n)| match_score(&item, &b.to_lowercase(), n, group))
                    .max()
                    .unwrap_or(0);
                if theirs >= ours {
                    scan.shared_skipped += 1;
                    continue;
                }
                if !nera_core::safety::is_trashable(&path) || !seen.insert(path.clone()) {
                    continue;
                }
                let size = tree_size(&path, cancel);
                scan.items.push(Leftover { path, kind, size });
            }
        }
    }
    scan.items
        .sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
    scan
}

/// How specifically `item` (lower-cased file name) belongs to an app: the
/// length of the matched key, or 0 when it does not match.
pub fn match_score(item: &str, bundle: &str, names: &[String], group_container: bool) -> usize {
    let stem = item.rsplit_once('.').map(|(s, _)| s).unwrap_or(item);
    let mut best = 0;
    if is_specific_bundle(bundle) {
        let by_bundle = item == bundle
            || stem == bundle
            || item
                .strip_prefix(bundle)
                .is_some_and(|rest| rest.starts_with('.'))
            || (group_container && item.ends_with(&format!(".{bundle}")));
        if by_bundle {
            best = bundle.len();
        }
    }
    for n in names {
        if is_specific_name(n) && (item == n || stem == n) {
            best = best.max(n.len());
        }
    }
    best
}

fn is_specific_bundle(bundle: &str) -> bool {
    bundle.len() >= 5 && bundle.contains('.')
}

/// Names too short or too generic to identify one app's folder.
fn is_specific_name(name: &str) -> bool {
    const GENERIC: [&str; 22] = [
        "app",
        "apps",
        "application",
        "applications",
        "helper",
        "electron",
        "google",
        "microsoft",
        "apple",
        "adobe",
        "jetbrains",
        "mozilla",
        "data",
        "cache",
        "caches",
        "logs",
        "library",
        "updater",
        "crashreporter",
        "preferences",
        "support",
        "default",
    ];
    name.chars().count() >= 3 && !GENERIC.contains(&name)
}

// MARK: - Uninstall

/// What the user confirmed.
#[derive(Clone, Debug)]
pub struct UninstallPlan {
    pub app: PathBuf,
    pub app_size: u64,
    pub leftovers: Vec<Leftover>,
}

impl UninstallPlan {
    pub fn total(&self) -> u64 {
        self.app_size + self.leftovers.iter().map(|l| l.size).sum::<u64>()
    }
}

#[derive(Debug, Default)]
pub struct UninstallReport {
    pub app_removed: bool,
    pub leftovers_removed: usize,
    pub freed: u64,
    /// Items that could not be moved, with the reason.
    pub failures: Vec<(PathBuf, String)>,
    /// A permission error blocked the app bundle (App Management / ownership).
    pub app_permission_denied: bool,
    /// The app was running when the uninstall started; nothing was touched.
    pub refused_running: bool,
}

/// Move the app to the Trash first; only when that succeeds, its selected
/// leftovers (which must lie inside `library`, normally `~/Library`) follow.
/// Refuses while the app runs or when it is protected.
pub fn uninstall(
    plan: &UninstallPlan,
    library: &Path,
    protection: Protection,
    is_running: impl Fn(&Path) -> bool,
    trash: impl Fn(&Path) -> io::Result<()>,
) -> UninstallReport {
    let mut report = UninstallReport::default();
    if protection != Protection::Removable {
        report.failures.push((
            plan.app.clone(),
            "This app is protected and can't be uninstalled.".into(),
        ));
        return report;
    }
    if is_running(&plan.app) {
        report.refused_running = true;
        report.failures.push((
            plan.app.clone(),
            "The app is running. Quit it first.".into(),
        ));
        return report;
    }
    match trash(&plan.app) {
        Ok(()) => {
            report.app_removed = true;
            report.freed += plan.app_size;
        }
        Err(err) => {
            report.app_permission_denied = is_permission_error(&err);
            report.failures.push((plan.app.clone(), err.to_string()));
            return report;
        }
    }
    for item in &plan.leftovers {
        // Leftovers must live in the user's Library; anything else is a bug.
        if !paths::is_under(&item.path, library) {
            report.failures.push((
                item.path.clone(),
                "Not in your Library folder; skipped.".into(),
            ));
            continue;
        }
        match trash(&item.path) {
            Ok(()) => {
                report.leftovers_removed += 1;
                report.freed += item.size;
            }
            Err(err) => report.failures.push((item.path.clone(), err.to_string())),
        }
    }
    report
}

fn is_permission_error(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::PermissionDenied {
        return true;
    }
    let msg = err.to_string().to_lowercase();
    msg.contains("permission")
        || msg.contains("not permitted")
        || msg.contains("don’t have permission")
}

/// Ask a running app to quit (it may ask to save documents first).
pub fn request_quit(bundle_id: &str) {
    if bundle_id.is_empty() {
        return;
    }
    let script = format!(
        "tell application id \"{}\" to quit",
        bundle_id.replace(['"', '\\'], "")
    );
    let _ = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("nera-apps-{tag}-{}", std::process::id()));
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

    fn make_app(dir: &Path, file: &str, bundle_id: &str, name: &str, version: &str) -> PathBuf {
        let app = dir.join(file);
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::create_dir_all(app.join("Contents/Resources")).unwrap();
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{bundle_id}</string>
<key>CFBundleExecutable</key><string>bin</string>
<key>CFBundleName</key><string>{name}</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
<key>CFBundleIconFile</key><string>Icon</string>
</dict></plist>"#
        );
        std::fs::write(app.join("Contents/Info.plist"), plist).unwrap();
        std::fs::write(app.join("Contents/Resources/Icon.icns"), b"icns").unwrap();
        std::fs::write(app.join("Contents/MacOS/bin"), vec![0u8; 50_000]).unwrap();
        app
    }

    fn app(name: &str, bundle: &str) -> InstalledApp {
        InstalledApp {
            path: PathBuf::from(format!("/Applications/{name}.app")),
            real_path: PathBuf::from(format!("/Applications/{name}.app")),
            executable: None,
            name: name.into(),
            bundle_id: bundle.into(),
            version: String::new(),
            alt_names: vec![],
            icon_file: None,
            protection: Protection::Removable,
            running: false,
            size: None,
            last_used: None,
            icon_png: None,
        }
    }

    #[test]
    fn discovers_apps_with_subfolders_and_protection() {
        let tmp = TempDir::new("discover");
        let apps_dir = tmp.0.join("Applications");
        let sys_dir = tmp.0.join("SystemApps");
        let slack = make_app(
            &apps_dir,
            "Slack.app",
            "com.tinyspeck.slackmacgap",
            "Slack",
            "4.41",
        );
        make_app(
            &apps_dir.join("Utilities"),
            "Tool.app",
            "com.example.tool",
            "Tool",
            "1.0",
        );
        make_app(
            &apps_dir.join("Deep/Deeper"),
            "TooDeep.app",
            "com.example.deep",
            "Deep",
            "1",
        );
        make_app(&apps_dir, "Xcode.app", "com.apple.dt.Xcode", "Xcode", "16");
        make_app(&apps_dir, "Nera Cleaner.app", paths::BUNDLE_ID, "Nera", "1");
        make_app(&sys_dir, "Notes.app", "com.example.notes", "Notes", "4");
        std::fs::create_dir_all(apps_dir.join("Broken.app")).unwrap();
        // An app whose executable is just a link to a program that is running elsewhere
        // (Claude Code's URL handler links to the `claude` CLI).
        let linked = make_app(&apps_dir, "Linked.app", "com.example.linked", "Linked", "1");
        let cli = tmp.0.join("cli-binary");
        std::fs::write(&cli, b"x").unwrap();
        std::fs::remove_file(linked.join("Contents/MacOS/bin")).unwrap();
        std::os::unix::fs::symlink(&cli, linked.join("Contents/MacOS/bin")).unwrap();
        std::os::unix::fs::symlink(&slack, apps_dir.join("Link.app")).unwrap();

        let roots = vec![
            AppRoot {
                path: apps_dir.clone(),
                system: false,
            },
            AppRoot {
                path: tmp.0.join("missing"),
                system: false,
            },
            AppRoot {
                path: sys_dir,
                system: true,
            },
        ];
        // Slack's main binary runs; Tool only has a helper tool running inside it.
        let running = RunningSet::from_paths(vec![
            slack.join("Contents/MacOS/bin"),
            apps_dir.join("Utilities/Tool.app/Contents/Developer/usr/bin/make"),
            cli.canonicalize().unwrap(),
        ]);
        let me = SelfIdentity {
            bundle_id: paths::BUNDLE_ID.into(),
            executable: None,
        };
        let found = discover(&roots, &me, &running);
        let names: Vec<&str> = found.apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            ["Linked", "Nera Cleaner", "Notes", "Slack", "Tool", "Xcode"]
        );
        assert!(found.unreadable.is_empty());

        let by = |n: &str| found.apps.iter().find(|a| a.name == n).unwrap();
        assert_eq!(by("Slack").protection, Protection::Removable);
        assert!(by("Slack").running);
        assert!(!by("Tool").running);
        assert!(
            !by("Linked").running,
            "a link to a running program is not the app running"
        );
        assert_eq!(by("Slack").version, "4.41");
        assert_eq!(
            by("Slack").icon_file.as_deref(),
            Some(slack.join("Contents/Resources/Icon.icns").as_path())
        );
        assert_eq!(by("Xcode").protection, Protection::Apple);
        assert_eq!(by("Notes").protection, Protection::Apple);
        assert_eq!(by("Nera Cleaner").protection, Protection::SelfApp);
        assert_eq!(by("Nera Cleaner").alt_names, vec!["Nera".to_string()]);
        assert!(app_size(&slack, &Cancel::new()) >= 50_000);
    }

    #[test]
    fn own_executable_marks_bundle_as_self() {
        let exe = std::env::current_exe().unwrap();
        let me = SelfIdentity {
            bundle_id: "x.y.z".into(),
            executable: exe.canonicalize().ok(),
        };
        let parent = exe.parent().unwrap();
        assert_eq!(
            protection_for(parent, "com.example.other", false, &me),
            Protection::SelfApp
        );
        assert_eq!(
            protection_for(Path::new("/Applications/Foo.app"), "com.foo", false, &me),
            Protection::Removable
        );
    }

    #[test]
    fn matches_leftovers_exactly_and_by_specificity() {
        let names = vec!["notes".to_string()];
        assert!(match_score("com.foo.app", "com.foo.app", &[], false) > 0);
        assert!(match_score("com.foo.app.plist", "com.foo.app", &[], false) > 0);
        assert!(match_score("com.foo.app.savedstate", "com.foo.app", &[], false) > 0);
        assert!(match_score("com.foo.app.helper", "com.foo.app", &[], false) > 0);
        assert_eq!(
            match_score("com.foo.application", "com.foo.app", &[], false),
            0
        );
        assert_eq!(
            match_score("abc123.com.foo.app", "com.foo.app", &[], false),
            0
        );
        assert!(match_score("abc123.com.foo.app", "com.foo.app", &[], true) > 0);
        assert!(match_score("notes", "", &names, false) > 0);
        assert_eq!(match_score("sticky notes", "", &names, false), 0);
        assert_eq!(match_score("google", "", &["google".into()], false), 0);
        assert_eq!(match_score("anything", "", &[], false), 0);
        // A more specific bundle id wins.
        assert!(
            match_score(
                "com.google.chrome.canary.plist",
                "com.google.chrome.canary",
                &[],
                false
            ) > match_score(
                "com.google.chrome.canary.plist",
                "com.google.chrome",
                &[],
                false
            )
        );
    }

    #[test]
    fn finds_leftovers_and_skips_shared_data() {
        let tmp = TempDir::new("leftovers");
        let lib = tmp.0.join("Library");
        let w = |rel: &str, len: usize| {
            let p = lib.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, vec![1u8; len]).unwrap();
        };
        w("Application Support/Code/state.db", 20_000);
        w("Caches/com.microsoft.VSCode/data", 10_000);
        w("Caches/com.microsoft.VSCode.ShipIt/x", 1_000);
        w("Preferences/com.microsoft.VSCode.plist", 100);
        w(
            "Preferences/ByHost/com.microsoft.VSCode.ABCD-1234.plist",
            100,
        );
        w(
            "Saved Application State/com.microsoft.VSCode.savedState/w",
            100,
        );
        w("Group Containers/UBF8T346G9.com.microsoft.VSCode/x", 100);
        w("Application Scripts/UBF8T346G9.com.microsoft.VSCode/x", 100);
        w("HTTPStorages/com.microsoft.VSCode/x", 100);
        w("LaunchAgents/com.microsoft.VSCode.update.plist", 100);
        // Not ours: substring, other app, shared bundle.
        w("Application Support/Code Helper/x", 100);
        w("Caches/com.microsoft.VSCodeInsiders/x", 100);
        w("Caches/com.microsoft.VSCode.Insiders/x", 100);
        w("Application Support/Shared/x", 100);

        let mut vscode = app("Visual Studio Code", "com.microsoft.VSCode");
        vscode.alt_names = vec!["Code".into()];
        let insiders = app("Insiders", "com.microsoft.VSCode.Insiders");
        let mut a = app("Shared", "com.example.shared");
        a.alt_names = vec![];
        let mut b = app("Shared", "com.example.shared");
        b.path = PathBuf::from("/Users/me/Applications/Shared.app");
        let all = vec![vscode.clone(), insiders, a.clone(), b];

        let scan = find_leftovers(&lib, &LeftoverQuery::for_app(&vscode, &all), &Cancel::new());
        let mut rels: Vec<String> = scan
            .items
            .iter()
            .map(|l| {
                l.path
                    .strip_prefix(&lib)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        rels.sort();
        assert_eq!(
            rels,
            [
                "Application Scripts/UBF8T346G9.com.microsoft.VSCode",
                "Application Support/Code",
                "Caches/com.microsoft.VSCode",
                "Caches/com.microsoft.VSCode.ShipIt",
                "Group Containers/UBF8T346G9.com.microsoft.VSCode",
                "HTTPStorages/com.microsoft.VSCode",
                "LaunchAgents/com.microsoft.VSCode.update.plist",
                "Preferences/ByHost/com.microsoft.VSCode.ABCD-1234.plist",
                "Preferences/com.microsoft.VSCode.plist",
                "Saved Application State/com.microsoft.VSCode.savedState",
            ]
        );
        assert_eq!(scan.shared_skipped, 1); // the Insiders cache
        assert_eq!(scan.items[0].path, lib.join("Application Support/Code"));
        assert!(scan.items[0].size >= 20_000);
        assert_eq!(scan.items[0].kind, LeftoverKind::ApplicationSupport);

        // Two installs with the same name share their data: nothing is claimed.
        let shared = find_leftovers(&lib, &LeftoverQuery::for_app(&a, &all), &Cancel::new());
        assert!(shared.items.is_empty());
        assert_eq!(shared.shared_skipped, 1);
    }

    #[test]
    fn uninstall_trashes_app_then_leftovers() {
        let plan = UninstallPlan {
            app: PathBuf::from("/tmp/x/Foo.app"),
            app_size: 1_000,
            leftovers: vec![
                Leftover {
                    path: PathBuf::from("/tmp/x/L/a"),
                    kind: LeftoverKind::Caches,
                    size: 10,
                },
                Leftover {
                    path: PathBuf::from("/tmp/x/L/b"),
                    kind: LeftoverKind::Logs,
                    size: 20,
                },
            ],
        };
        assert_eq!(plan.total(), 1_030);
        let trashed = RefCell::new(Vec::new());
        let report = uninstall(
            &plan,
            Path::new("/tmp/x"),
            Protection::Removable,
            |_| false,
            |p| {
                trashed.borrow_mut().push(p.to_path_buf());
                if p.ends_with("b") {
                    Err(io::Error::other("busy"))
                } else {
                    Ok(())
                }
            },
        );
        assert!(report.app_removed);
        assert_eq!(report.leftovers_removed, 1);
        assert_eq!(report.freed, 1_010);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(trashed.borrow()[0], plan.app);
    }

    #[test]
    fn uninstall_refuses_running_protected_and_stops_on_app_failure() {
        let plan = UninstallPlan {
            app: PathBuf::from("/tmp/x/Foo.app"),
            app_size: 1,
            leftovers: vec![Leftover {
                path: PathBuf::from("/tmp/x/a"),
                kind: LeftoverKind::Caches,
                size: 1,
            }],
        };
        let calls = RefCell::new(0);
        let count = |_: &Path| {
            *calls.borrow_mut() += 1;
            Ok(())
        };
        let r = uninstall(
            &plan,
            Path::new("/tmp/x"),
            Protection::Removable,
            |_| true,
            count,
        );
        assert!(r.refused_running && !r.app_removed);
        let r = uninstall(
            &plan,
            Path::new("/tmp/x"),
            Protection::Apple,
            |_| false,
            count,
        );
        assert!(!r.app_removed && r.failures.len() == 1);
        let r = uninstall(
            &plan,
            Path::new("/tmp/x"),
            Protection::SelfApp,
            |_| false,
            count,
        );
        assert!(!r.app_removed);
        assert_eq!(*calls.borrow(), 0);

        let r = uninstall(
            &plan,
            Path::new("/tmp/x"),
            Protection::Removable,
            |_| false,
            |p| {
                *calls.borrow_mut() += 1;
                assert_eq!(
                    p,
                    plan.app.as_path(),
                    "leftovers must not be touched after the app fails"
                );
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
            },
        );
        assert!(!r.app_removed && r.app_permission_denied);
        assert_eq!(*calls.borrow(), 1);
    }

    #[test]
    fn filters_and_sorts() {
        let now = SystemTime::now();
        let mut a = app("Zed", "dev.zed.Zed");
        a.size = Some(10);
        a.last_used = Some(now - Duration::from_secs(86_400));
        let mut b = app("arc", "company.thebrowser.Browser");
        b.size = Some(50);
        b.last_used = Some(now - Duration::from_secs(400 * 86_400));
        let mut c = app("Safari", "com.apple.Safari");
        c.protection = Protection::Apple;
        let d = app("Mail Helper", "com.example.mail");
        let apps = vec![a, b, c, d];

        assert_eq!(
            filter_sort(&apps, "", SortKey::Name, false, now),
            vec![1, 3, 0]
        );
        assert_eq!(
            filter_sort(&apps, "", SortKey::Name, true, now),
            vec![1, 3, 2, 0]
        );
        assert_eq!(
            filter_sort(&apps, "", SortKey::Size, false, now),
            vec![1, 0, 3]
        );
        assert_eq!(
            filter_sort(&apps, "", SortKey::LastUsed, false, now),
            vec![1, 0, 3]
        );
        assert_eq!(filter_sort(&apps, "ZED", SortKey::Name, true, now), vec![0]);
        assert_eq!(
            filter_sort(&apps, "thebrowser", SortKey::Name, true, now),
            vec![1]
        );
        assert!(filter_sort(&apps, "nothing", SortKey::Name, true, now).is_empty());

        assert!(unused_for(&apps[1], 6, now));
        assert!(!unused_for(&apps[0], 6, now));
        assert!(!unused_for(&apps[3], 6, now));
    }

    #[test]
    fn parses_spotlight_dates() {
        let t = parse_mdls_date("2026-11-09 16:29:23 +0000").unwrap();
        assert_eq!(
            t.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_794_241_763
        );
        let shifted = parse_mdls_date("2026-11-09 18:29:23 +0200").unwrap();
        assert_eq!(t, shifted);
        assert!(parse_mdls_date("(null)").is_none());
        let v = parse_mdls_raw("2026-11-09 16:29:23 +0000\0(null)\x002020-01-01 00:00:00 +0000");
        assert_eq!(v.len(), 3);
        assert!(v[0].is_some() && v[1].is_none() && v[2].is_some());
        assert!(parse_mdls_raw("").is_empty());
    }

    #[test]
    fn describes_last_used_in_calendar_days_and_months() {
        use chrono::{Local, TimeZone};
        let now: SystemTime = Local
            .with_ymd_and_hms(2026, 11, 10, 12, 0, 0)
            .unwrap()
            .into();
        let at = |y, m, d| -> Option<SystemTime> {
            Some(Local.with_ymd_and_hms(y, m, d, 9, 30, 0).unwrap().into())
        };
        assert_eq!(describe_last_used(None, now), "No usage data");
        assert_eq!(describe_last_used(at(2026, 11, 10), now), "Used today");
        assert_eq!(describe_last_used(at(2026, 11, 9), now), "Used yesterday");
        assert_eq!(
            describe_last_used(at(2026, 10, 29), now),
            "Used 12 days ago"
        );
        assert_eq!(
            describe_last_used(at(2026, 10, 11), now),
            "Used 30 days ago"
        );
        assert_eq!(
            describe_last_used(at(2026, 10, 10), now),
            "Used a month ago"
        );
        assert_eq!(
            describe_last_used(at(2026, 5, 11), now),
            "Used 5 months ago"
        );
        assert_eq!(
            describe_last_used(at(2026, 5, 10), now),
            "Used 6 months ago"
        );
        assert_eq!(
            describe_last_used(at(2025, 11, 11), now),
            "Used 11 months ago"
        );
        assert_eq!(describe_last_used(at(2025, 11, 10), now), "Used a year ago");
        assert_eq!(
            describe_last_used(at(2024, 11, 10), now),
            "Used 2 years ago"
        );
        // The tile's "6 months" and the row's text agree on the boundary.
        let mut a = app("A", "com.example.a");
        a.last_used = at(2026, 5, 10);
        assert!(unused_for(&a, 6, now));
        a.last_used = at(2026, 5, 11);
        assert!(!unused_for(&a, 6, now));
    }

    #[test]
    fn sizes_hard_links_once_like_du() {
        let tmp = TempDir::new("links");
        let dir = tmp.0.join("X.app");
        std::fs::create_dir_all(dir.join("Contents/a")).unwrap();
        std::fs::create_dir_all(dir.join("Contents/b")).unwrap();
        let one = dir.join("Contents/a/lib");
        std::fs::write(&one, vec![5u8; 200_000]).unwrap();
        std::fs::hard_link(&one, dir.join("Contents/b/lib")).unwrap();
        std::fs::write(dir.join("Contents/.hidden"), vec![1u8; 100_000]).unwrap();
        std::os::unix::fs::symlink(&one, dir.join("Contents/link")).unwrap();

        let size = tree_size(&dir, &Cancel::new());
        assert!(size >= 300_000, "hidden file counted: {size}");
        assert!(
            size < 400_000,
            "hard link counted once and symlink not followed: {size}"
        );
    }

    #[test]
    fn sees_this_process_by_the_path_it_was_started_with() {
        let me = std::process::id() as i32;
        assert!(process_ids().contains(&me));
        let started = launch_path(me).expect("own exec path");
        assert!(started.is_absolute());
        let set = RunningSet::snapshot();
        assert!(set.exes.contains(&started));
        assert!(set
            .exes
            .contains(&std::env::current_exe().unwrap().canonicalize().unwrap()));
    }

    #[test]
    fn compressed_files_use_the_size_finder_reports() {
        // Values are what NSURL (Finder, Spotlight) reports for these files.
        let probe = |path: &str, expected: u64| {
            use std::os::macos::fs::MetadataExt;
            let path = Path::new(path);
            let Ok(meta) = std::fs::symlink_metadata(path) else {
                return;
            };
            if meta.st_flags() & UF_COMPRESSED == 0 {
                return; // not the file this was measured on
            }
            assert_eq!(
                allocated(|| path.to_path_buf(), &meta),
                expected,
                "{}",
                path.display()
            );
            assert_eq!(
                tree_size(path, &Cancel::new()),
                expected,
                "{}",
                path.display()
            );
        };
        // Payload stored in the file's metadata: nothing allocated, though st_blocks says 4 KB.
        probe(
            "/Applications/Telegram.app/Contents/Resources/currencies.json",
            0,
        );
        probe("/usr/share/misc/ascii", 4096);
    }

    #[test]
    fn reads_real_applications_folder_read_only() {
        // Read-only smoke test against the real system: listing must not panic.
        let found = discover(
            &default_roots(),
            &SelfIdentity::current(),
            &RunningSet::default(),
        );
        for app in &found.apps {
            assert!(app.path.is_absolute());
            if app.bundle_id.to_lowercase().starts_with("com.apple.") {
                assert_ne!(app.protection, Protection::Removable);
            }
        }
    }

    #[test]
    fn converts_icns_into_cache() {
        let tmp = TempDir::new("icons");
        // A real icns from macOS (read-only source); conversion writes only to tmp.
        let src = Path::new("/System/Applications/Calculator.app/Contents/Resources/AppIcon.icns");
        if !src.is_file() {
            return;
        }
        let png = icon_png(src, &tmp.0).expect("converted");
        assert!(png.is_file() && png.starts_with(&tmp.0));
        assert_eq!(icon_png(src, &tmp.0), Some(png));
    }
}
