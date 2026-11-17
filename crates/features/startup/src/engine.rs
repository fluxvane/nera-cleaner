//! Startup engine: launch agents/daemons, login items and the launchctl
//! commands that turn user agents on or off. Port of the Swift
//! `StartupManager`, extended with plist parsing and launchd state.
//!
//! Everything that reads state is read-only (`launchctl print`,
//! `launchctl print-disabled`, plist parsing, `osascript` reading login item
//! names). Changes are expressed as a [`Step`] plan by [`plan_toggle`] /
//! [`plan_remove`] and only run by [`execute`], so tests can check the plan
//! without touching launchd.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// Where a launchd job is installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    /// `~/Library/LaunchAgents`: runs in the user's session, user-editable.
    User,
    /// `/Library/LaunchAgents`: runs in every user's session, admin-owned.
    GlobalAgent,
    /// `/Library/LaunchDaemons`: runs as root at boot, admin-owned.
    Daemon,
}

impl Scope {
    pub fn kind_label(self) -> &'static str {
        match self {
            Scope::User | Scope::GlobalAgent => "Agent",
            Scope::Daemon => "Daemon",
        }
    }

    pub fn is_user(self) -> bool {
        self == Scope::User
    }
}

/// When launchd starts the job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// `KeepAlive` is true: launchd keeps it running (restarts it).
    AlwaysOn,
    /// `KeepAlive` is a dictionary of conditions (`SuccessfulExit`, `PathState`...):
    /// launchd restarts it only while they hold, so it is not always on.
    Conditional,
    /// `RunAtLoad`: started once at login / boot.
    AtLogin,
    /// `StartInterval` / `StartCalendarInterval`.
    Scheduled,
    /// `WatchPaths` / `QueueDirectories`: started when those files change.
    FileChange,
    /// Started on demand (Mach services, sockets...).
    OnDemand,
}

impl Trigger {
    pub fn label(self, scope: Scope) -> &'static str {
        match self {
            Trigger::AlwaysOn => "Always on",
            Trigger::Conditional => "Restarts on conditions",
            Trigger::AtLogin if scope == Scope::Daemon => "At startup",
            Trigger::AtLogin => "At login",
            Trigger::Scheduled => "Scheduled",
            Trigger::FileChange => "On file change",
            Trigger::OnDemand => "On demand",
        }
    }
}

/// Fields of a launchd property list that matter to the user.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlistInfo {
    pub label: Option<String>,
    pub program: Option<String>,
    pub arguments: Vec<String>,
    /// `BundleProgram` (path relative to the owning app, SMAppService jobs).
    pub bundle_program: Option<String>,
    pub bundle_ids: Vec<String>,
    pub run_at_load: bool,
    /// `KeepAlive` is `true`.
    pub keep_alive: bool,
    /// `KeepAlive` is a non-empty dictionary of conditions.
    pub keep_alive_conditional: bool,
    pub scheduled: bool,
    /// `WatchPaths` or `QueueDirectories` present.
    pub watches_files: bool,
    pub disabled: bool,
}

impl PlistInfo {
    /// Executable launchd runs: `Program`, else `ProgramArguments[0]`.
    pub fn executable(&self) -> Option<&str> {
        self.program
            .as_deref()
            .or_else(|| self.arguments.first().map(String::as_str))
    }

    /// Every way launchd starts the job, strongest first. `KeepAlive: true`
    /// makes the rest irrelevant; a job with none of them runs on demand.
    pub fn triggers(&self) -> Vec<Trigger> {
        if self.keep_alive {
            return vec![Trigger::AlwaysOn];
        }
        let mut out = Vec::new();
        if self.keep_alive_conditional {
            out.push(Trigger::Conditional);
        }
        if self.run_at_load {
            out.push(Trigger::AtLogin);
        }
        if self.scheduled {
            out.push(Trigger::Scheduled);
        }
        if self.watches_files {
            out.push(Trigger::FileChange);
        }
        if out.is_empty() {
            out.push(Trigger::OnDemand);
        }
        out
    }
}

/// Runtime state of a loaded job from `launchctl print <domain>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loaded {
    /// `None` when loaded but not running.
    pub pid: Option<i32>,
    pub last_exit: Option<i32>,
}

/// One launch agent or daemon.
#[derive(Clone, Debug)]
pub struct LaunchItem {
    /// Stable id: the plist path.
    pub id: String,
    pub plist: PathBuf,
    pub scope: Scope,
    pub label: String,
    pub info: Option<PlistInfo>,
    /// Why the plist could not be read (permissions, malformed).
    pub read_error: Option<String>,
    pub vendor: String,
    pub is_apple: bool,
    /// Hidden `.name.plist` file left by the old app's "disable".
    pub legacy_hidden: bool,
    /// `launchctl disable/enable` override: `Some(true)` = disabled.
    pub override_disabled: Option<bool>,
    /// `None` when launchd state could not be read.
    pub loaded: Option<Option<Loaded>>,
    /// The executable is an absolute path that no longer exists.
    pub program_missing: bool,
}

impl LaunchItem {
    pub fn enabled(&self) -> bool {
        if self.legacy_hidden {
            return false;
        }
        match self.override_disabled {
            Some(disabled) => !disabled,
            None => !self.info.as_ref().is_some_and(|i| i.disabled),
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.loaded, Some(Some(Loaded { pid: Some(_), .. })))
    }

    pub fn is_loaded(&self) -> bool {
        matches!(self.loaded, Some(Some(_)))
    }

    pub fn status(&self) -> Status {
        if self.is_running() {
            Status::Running
        } else if self.is_loaded() {
            Status::Loaded
        } else if !self.enabled() {
            Status::Disabled
        } else if self.loaded.is_some() {
            Status::NotLoaded
        } else {
            Status::Unknown
        }
    }

    /// User agents that are not Apple's can be changed without admin rights.
    pub fn is_editable(&self) -> bool {
        self.scope.is_user() && !self.is_apple && self.read_error.is_none()
    }

    /// `"At login, Scheduled"`; `None` when the file could not be read.
    pub fn trigger_label(&self) -> Option<String> {
        let info = self.info.as_ref()?;
        let labels: Vec<&str> = info
            .triggers()
            .into_iter()
            .map(|t| t.label(self.scope))
            .collect();
        Some(labels.join(", "))
    }

    /// launchd needs a `Label`; a property list without one is never loaded.
    pub fn has_no_label(&self) -> bool {
        self.info.as_ref().is_some_and(|i| i.label.is_none())
    }

    pub fn executable(&self) -> Option<&str> {
        self.info.as_ref().and_then(PlistInfo::executable)
    }

    /// Lower-case haystack for search.
    pub fn matches(&self, query: &str) -> bool {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return true;
        }
        [
            Some(self.label.as_str()),
            Some(self.vendor.as_str()),
            self.executable(),
        ]
        .into_iter()
        .flatten()
        .any(|s| s.to_lowercase().contains(&q))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Running,
    Loaded,
    Disabled,
    NotLoaded,
    Unknown,
}

/// Home-relative display form of a path: `/Users/me/Library/x` → `~/Library/x`.
pub fn tilde(path: &str) -> String {
    let home = nera_core::paths::home();
    match Path::new(path).strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.to_string(),
    }
}

impl LaunchItem {
    /// One line under the label: vendor · when it starts · program (or why the
    /// file could not be read).
    pub fn subtitle(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.vendor != "Unknown" {
            parts.push(self.vendor.clone());
        }
        if self.has_no_label() {
            parts.push("No Label key, launchd ignores this file".to_string());
            return parts.join(" \u{b7} ");
        }
        if let Some(trigger) = self.trigger_label() {
            parts.push(trigger);
        }
        match (&self.read_error, self.executable()) {
            (Some(err), _) => parts.push(err.clone()),
            (None, Some(exe)) => parts.push(tilde(exe)),
            (None, None) => parts.push(
                self.info
                    .as_ref()
                    .and_then(|i| i.bundle_program.clone())
                    .unwrap_or_else(|| "No program".into()),
            ),
        }
        parts.join(" \u{b7} ")
    }
}

/// Items matching the search `query`, hiding Apple's unless `show_apple`.
pub fn visible<'a>(
    items: impl Iterator<Item = &'a LaunchItem>,
    query: &str,
    show_apple: bool,
) -> Vec<&'a LaunchItem> {
    items
        .filter(|i| (show_apple || !i.is_apple) && i.matches(query))
        .collect()
}

/// Result of [`scan`].
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub items: Vec<LaunchItem>,
    /// Folders that could not be listed (path, error, permission problem?).
    pub folder_errors: Vec<(PathBuf, String, bool)>,
    /// launchctl could not be queried; loaded state is unknown.
    pub launchd_error: Option<String>,
}

impl Snapshot {
    pub fn user(&self) -> impl Iterator<Item = &LaunchItem> {
        self.items.iter().filter(|i| i.scope.is_user())
    }

    pub fn system(&self) -> impl Iterator<Item = &LaunchItem> {
        self.items.iter().filter(|i| !i.scope.is_user())
    }
}

// ---------------------------------------------------------------------------
// Plist parsing
// ---------------------------------------------------------------------------

pub fn parse_plist_bytes(bytes: &[u8]) -> Result<PlistInfo, String> {
    let value =
        plist::Value::from_reader(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
    from_value(&value)
}

pub fn parse_plist_file(path: &Path) -> Result<PlistInfo, String> {
    let value = plist::Value::from_file(path).map_err(|e| match e.as_io() {
        Some(io) if io.kind() == std::io::ErrorKind::PermissionDenied => {
            "Only an administrator can read this file".to_string()
        }
        Some(io) if io.kind() == std::io::ErrorKind::NotFound => {
            "The file it links to is missing".to_string()
        }
        Some(io) => io.to_string(),
        None => format!("Not a valid property list ({e})"),
    })?;
    from_value(&value)
}

fn from_value(value: &plist::Value) -> Result<PlistInfo, String> {
    let dict = value.as_dictionary().ok_or("Not a launchd property list")?;
    let string = |key: &str| {
        dict.get(key)
            .and_then(plist::Value::as_string)
            .map(str::to_owned)
    };
    let strings = |key: &str| -> Vec<String> {
        dict.get(key)
            .and_then(plist::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(plist::Value::as_string)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let flag = |key: &str| {
        dict.get(key)
            .and_then(plist::Value::as_boolean)
            .unwrap_or(false)
    };
    let keep_alive = matches!(dict.get("KeepAlive"), Some(plist::Value::Boolean(true)));
    // A dictionary (SuccessfulExit, PathState...) restarts the job only while its conditions hold.
    let keep_alive_conditional =
        matches!(dict.get("KeepAlive"), Some(plist::Value::Dictionary(d)) if !d.is_empty());
    let mut bundle_ids = strings("AssociatedBundleIdentifiers");
    if bundle_ids.is_empty() {
        // Single-string form is also accepted by launchd.
        bundle_ids.extend(string("AssociatedBundleIdentifiers"));
    }
    Ok(PlistInfo {
        label: string("Label"),
        program: string("Program"),
        arguments: strings("ProgramArguments"),
        bundle_program: string("BundleProgram"),
        bundle_ids,
        run_at_load: flag("RunAtLoad"),
        keep_alive,
        keep_alive_conditional,
        scheduled: dict.contains_key("StartInterval") || dict.contains_key("StartCalendarInterval"),
        watches_files: dict.contains_key("WatchPaths") || dict.contains_key("QueueDirectories"),
        disabled: flag("Disabled"),
    })
}

// ---------------------------------------------------------------------------
// Ownership
// ---------------------------------------------------------------------------

pub fn is_apple(label: &str, executable: Option<&str>) -> bool {
    label.starts_with("com.apple.") || executable.is_some_and(|e| e.starts_with("/System/"))
}

/// App bundle name in a path: `/Applications/Kiro CLI.app/Contents/…` → "Kiro CLI".
pub fn app_name_in(path: &str) -> Option<String> {
    Path::new(path).components().find_map(|c| {
        let s = c.as_os_str().to_str()?;
        s.strip_suffix(".app")
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
    })
}

/// Who installed the job, best effort: owning app, package manager, or the
/// organisation in a reverse-DNS label/bundle id.
pub fn vendor(label: &str, executable: Option<&str>, bundle_ids: &[String]) -> String {
    if let Some(app) = executable.and_then(app_name_in) {
        return app;
    }
    let lower = label.to_lowercase();
    if lower.starts_with("homebrew.") || lower.starts_with("sh.brew.") {
        return "Homebrew".into();
    }
    if let Some(exe) = executable {
        if exe.starts_with("/opt/homebrew/")
            || exe.starts_with("/usr/local/Cellar/")
            || exe.starts_with("/usr/local/opt/")
        {
            return "Homebrew".into();
        }
    }
    let ids = std::iter::once(label).chain(bundle_ids.iter().map(String::as_str));
    for id in ids {
        if let Some(org) = org_from_reverse_dns(id) {
            return org;
        }
    }
    "Unknown".into()
}

fn org_from_reverse_dns(id: &str) -> Option<String> {
    const TLDS: &[&str] = &[
        "com", "org", "net", "io", "co", "us", "de", "uk", "fr", "jp", "sh", "app", "dev", "me",
        "ai", "info", "edu", "ch", "nl", "se", "ca", "au", "eu", "tv", "cc", "so",
    ];
    let mut parts = id.split('.');
    // Team-id prefixed labels: "S8EX82NJP6.com.vendor.app".
    let mut first = parts.next()?;
    if first.len() == 10
        && first
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        first = parts.next()?;
    }
    if !TLDS.contains(&first.to_lowercase().as_str()) {
        return None;
    }
    let org = parts.next().filter(|s| !s.is_empty())?;
    Some(pretty_org(org))
}

fn pretty_org(org: &str) -> String {
    const KNOWN: &[(&str, &str)] = &[
        ("apple", "Apple"),
        ("adobe", "Adobe"),
        ("microsoft", "Microsoft"),
        ("google", "Google"),
        ("jetbrains", "JetBrains"),
        ("macpaw", "MacPaw"),
        ("rogueamoeba", "Rogue Amoeba"),
        ("openvpn", "OpenVPN"),
        ("tunnelblick", "Tunnelblick"),
        ("amazon", "Amazon"),
        ("docker", "Docker"),
        ("zoom", "Zoom"),
        ("oracle", "Oracle"),
        ("dropbox", "Dropbox"),
        ("getdropbox", "Dropbox"),
        ("spotify", "Spotify"),
        ("elastic", "Elastic"),
        ("globaldelight", "Global Delight"),
        ("pqrs", "Karabiner"),
        ("wireguard", "WireGuard"),
        ("fortinet", "Fortinet"),
        ("ollama", "Ollama"),
        ("github", "GitHub"),
        ("vmware", "VMware"),
        ("teamviewer", "TeamViewer"),
        ("logi", "Logitech"),
        ("logitech", "Logitech"),
    ];
    let lower = org.to_lowercase();
    if let Some((_, name)) = KNOWN.iter().find(|(k, _)| *k == lower) {
        return (*name).to_string();
    }
    let mut chars = org.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// launchd state (read-only)
// ---------------------------------------------------------------------------

/// Parse the `services = { pid status label }` block of `launchctl print`.
pub fn parse_print_services(output: &str) -> HashMap<String, Loaded> {
    let mut map = HashMap::new();
    let mut inside = false;
    for line in output.lines() {
        let t = line.trim();
        if !inside {
            inside = t == "services = {";
            continue;
        }
        if t == "}" {
            break;
        }
        let mut tokens = t.split_whitespace();
        let (Some(pid), Some(status)) = (tokens.next(), tokens.next()) else {
            continue;
        };
        let label = tokens.collect::<Vec<_>>().join(" ");
        if label.is_empty() {
            continue;
        }
        let pid = pid.parse::<i32>().ok().filter(|p| *p > 0);
        let last_exit = status.parse::<i32>().ok();
        map.insert(label, Loaded { pid, last_exit });
    }
    map
}

/// Parse `launchctl list` (PID, Status, Label columns) as a fallback.
pub fn parse_list(output: &str) -> HashMap<String, Loaded> {
    output
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut cols = line.split('\t');
            let pid = cols.next()?.trim().parse::<i32>().ok().filter(|p| *p > 0);
            let last_exit = cols.next()?.trim().parse::<i32>().ok();
            let label = cols.next()?.trim();
            (!label.is_empty()).then(|| (label.to_string(), Loaded { pid, last_exit }))
        })
        .collect()
}

/// Parse `launchctl print-disabled <domain>`: label → disabled?
pub fn parse_print_disabled(output: &str) -> HashMap<String, bool> {
    output
        .lines()
        .filter_map(|line| {
            let (key, value) = line.trim().split_once("=>")?;
            let label = key.trim().trim_matches('"');
            let disabled = match value.trim() {
                "disabled" | "true" => true,
                "enabled" | "false" => false,
                _ => return None,
            };
            (!label.is_empty()).then(|| (label.to_string(), disabled))
        })
        .collect()
}

pub fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

pub fn gui_domain(uid: u32) -> String {
    format!("gui/{uid}")
}

fn launchctl_read(args: &[&str]) -> Result<String, String> {
    let out = Command::new("/bin/launchctl")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Couldn't run launchctl: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(first_line(
            &String::from_utf8_lossy(&out.stderr),
            "launchctl failed",
        ))
    }
}

fn first_line(s: &str, fallback: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

struct LaunchdState {
    gui: Result<HashMap<String, Loaded>, String>,
    system: Result<HashMap<String, Loaded>, String>,
    gui_overrides: Result<HashMap<String, bool>, String>,
    system_overrides: Result<HashMap<String, bool>, String>,
}

fn read_launchd_state() -> LaunchdState {
    let gui = gui_domain(uid());
    let gui_loaded = launchctl_read(&["print", &gui])
        .map(|o| parse_print_services(&o))
        .or_else(|e| {
            launchctl_read(&["list"])
                .map(|o| parse_list(&o))
                .map_err(|_| e)
        });
    LaunchdState {
        gui: gui_loaded,
        system: launchctl_read(&["print", "system"]).map(|o| parse_print_services(&o)),
        gui_overrides: launchctl_read(&["print-disabled", &gui]).map(|o| parse_print_disabled(&o)),
        system_overrides: launchctl_read(&["print-disabled", "system"])
            .map(|o| parse_print_disabled(&o)),
    }
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

pub fn folders() -> [(PathBuf, Scope); 3] {
    [
        (
            nera_core::paths::home_join("Library/LaunchAgents"),
            Scope::User,
        ),
        (PathBuf::from("/Library/LaunchAgents"), Scope::GlobalAgent),
        (PathBuf::from("/Library/LaunchDaemons"), Scope::Daemon),
    ]
}

/// True only when the path really does not exist. A path we are merely not
/// allowed to look at (macOS privacy folders) is not reported as missing.
fn path_is_missing(path: &Path) -> bool {
    matches!(std::fs::metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

/// Read plists in `dir` (no launchd state). `Err((message, permission))`.
pub fn read_folder(dir: &Path, scope: Scope) -> Result<Vec<LaunchItem>, (String, bool)> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err((
                e.to_string(),
                e.kind() == std::io::ErrorKind::PermissionDenied,
            ))
        }
    };
    let mut items = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(file) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(stem) = file.strip_suffix(".plist") else {
            continue;
        };
        // Hidden ".name.plist" files are agents the old app "disabled" by renaming.
        let legacy_hidden = stem.starts_with('.');
        if legacy_hidden && !scope.is_user() {
            continue;
        }
        let stem = stem.trim_start_matches('.');
        if stem.is_empty() {
            continue;
        }
        let (info, read_error) = match parse_plist_file(&path) {
            Ok(i) => (Some(i), None),
            Err(e) => (None, Some(e)),
        };
        let label = info
            .as_ref()
            .and_then(|i| i.label.clone())
            .unwrap_or_else(|| stem.to_string());
        let executable = info
            .as_ref()
            .and_then(|i| i.executable())
            .map(str::to_owned);
        let bundle_ids = info
            .as_ref()
            .map(|i| i.bundle_ids.clone())
            .unwrap_or_default();
        let program_missing = info.as_ref().is_some_and(|i| i.bundle_program.is_none())
            && executable
                .as_deref()
                .is_some_and(|e| e.starts_with('/') && path_is_missing(Path::new(e)));
        items.push(LaunchItem {
            id: path.to_string_lossy().into_owned(),
            plist: path.clone(),
            scope,
            vendor: vendor(&label, executable.as_deref(), &bundle_ids),
            is_apple: is_apple(&label, executable.as_deref()),
            label,
            info,
            read_error,
            legacy_hidden,
            override_disabled: None,
            loaded: None,
            program_missing,
        });
    }
    Ok(items)
}

/// Every launch agent/daemon with its launchd state. Read-only.
pub fn scan() -> Snapshot {
    let mut snap = Snapshot::default();
    for (dir, scope) in folders() {
        match read_folder(&dir, scope) {
            Ok(items) => snap.items.extend(items),
            Err((msg, perm)) => snap.folder_errors.push((dir, msg, perm)),
        }
    }
    let state = read_launchd_state();
    apply_state(&mut snap, &state);
    sort_items(&mut snap.items);
    snap
}

fn apply_state(snap: &mut Snapshot, state: &LaunchdState) {
    let mut errors = Vec::new();
    for item in &mut snap.items {
        let (loaded, overrides) = match item.scope {
            Scope::User | Scope::GlobalAgent => (&state.gui, &state.gui_overrides),
            Scope::Daemon => (&state.system, &state.system_overrides),
        };
        if item.has_no_label() {
            // launchd never loads it; looking it up by file name would match someone else's job.
            item.loaded = Some(None);
            continue;
        }
        match overrides {
            Ok(map) => item.override_disabled = map.get(&item.label).copied(),
            Err(e) => errors.push(e.clone()),
        }
        item.loaded = match loaded {
            Ok(map) => Some(map.get(&item.label).copied()),
            Err(e) => {
                errors.push(e.clone());
                None
            }
        };
    }
    errors.dedup();
    snap.launchd_error = errors.into_iter().next();
}

/// Third-party first, then by vendor and label.
pub fn sort_items(items: &mut [LaunchItem]) {
    items.sort_by(|a, b| {
        a.is_apple
            .cmp(&b.is_apple)
            .then_with(|| a.vendor.to_lowercase().cmp(&b.vendor.to_lowercase()))
            .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
    });
}

// ---------------------------------------------------------------------------
// Login items (System Events, read-only)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginItem {
    pub name: String,
    pub path: String,
    pub hidden: bool,
}

impl LoginItem {
    /// Stable id: path when known, else the name.
    pub fn id(&self) -> String {
        if self.path.is_empty() {
            format!("login-{}", self.name)
        } else {
            format!("login-{}", self.path)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginError {
    /// Automation permission for System Events was denied (-1743).
    NotAuthorized,
    TimedOut,
    Failed(String),
}

const LOGIN_ITEMS_SCRIPT: &str = r#"tell application "System Events"
set out to ""
repeat with li in login items
set out to out & (name of li) & tab & (path of li) & tab & (hidden of li) & linefeed
end repeat
return out
end tell"#;

/// Parse the tab-separated lines printed by [`LOGIN_ITEMS_SCRIPT`].
pub fn parse_login_items(output: &str) -> Vec<LoginItem> {
    let mut items: Vec<LoginItem> = output
        .lines()
        .filter_map(|line| {
            let mut cols = line.split('\t');
            let name = cols.next()?.trim();
            if name.is_empty() {
                return None;
            }
            let path = cols
                .next()
                .map(str::trim)
                .filter(|p| *p != "missing value")
                .unwrap_or("");
            let hidden = cols.next().is_some_and(|h| h.trim() == "true");
            Some(LoginItem {
                name: name.to_string(),
                path: path.to_string(),
                hidden,
            })
        })
        .collect();
    items.sort_by_key(|i| i.name.to_lowercase());
    items.dedup_by(|a, b| a.id() == b.id());
    items
}

pub fn classify_osascript_error(stderr: &str) -> LoginError {
    if stderr.contains("-1743")
        || stderr.contains("Not authorized")
        || stderr.contains("not allowed")
    {
        LoginError::NotAuthorized
    } else {
        LoginError::Failed(first_line(stderr, "osascript failed"))
    }
}

/// The user's login items via System Events. macOS may show an Automation
/// permission prompt the first time; we wait up to `timeout` for it.
pub fn read_login_items(timeout: Duration) -> Result<Vec<LoginItem>, LoginError> {
    let mut child = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(LOGIN_ITEMS_SCRIPT)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| LoginError::Failed(format!("Couldn't run osascript: {e}")))?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                // Our own child process; stop waiting for it.
                let _ = child.kill();
                let _ = child.wait();
                return Err(LoginError::TimedOut);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(LoginError::Failed(e.to_string())),
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut stdout);
    }
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr);
    }
    if status.success() {
        Ok(parse_login_items(&stdout))
    } else {
        Err(classify_osascript_error(&stderr))
    }
}

// ---------------------------------------------------------------------------
// Changes (planned, then executed)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// `launchctl <args>`; `what` describes the step for error messages.
    Launchctl {
        args: Vec<String>,
        what: &'static str,
    },
    /// Rename a legacy hidden `.name.plist` back to `name.plist`.
    Rename { from: PathBuf, to: PathBuf },
    /// Move the plist to the Trash via `nera_core::fs_util::trash_item`.
    Trash(PathBuf),
}

fn refuse(item: &LaunchItem) -> Result<(), String> {
    if !item.scope.is_user() {
        return Err("System items need an administrator to change.".into());
    }
    if item.is_apple {
        return Err("Apple items can't be changed here.".into());
    }
    if item.read_error.is_some() {
        return Err("This file can't be read.".into());
    }
    Ok(())
}

/// Steps to turn a user agent on (`enable`) or off. Off = persistent
/// `launchctl disable` + stop it now; on = `enable` + load it now.
pub fn plan_toggle(item: &LaunchItem, enable: bool, uid: u32) -> Result<Vec<Step>, String> {
    refuse(item)?;
    if item.has_no_label() {
        return Err("launchd ignores this file, so there is nothing to turn on or off.".into());
    }
    let domain = gui_domain(uid);
    let target = format!("{domain}/{}", item.label);
    let mut steps = Vec::new();
    if enable {
        let mut plist = item.plist.clone();
        if item.legacy_hidden {
            let name = item
                .plist
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let visible = item.plist.with_file_name(name.trim_start_matches('.'));
            steps.push(Step::Rename {
                from: item.plist.clone(),
                to: visible.clone(),
            });
            plist = visible;
        }
        steps.push(Step::Launchctl {
            args: vec!["enable".into(), target],
            what: "Couldn't turn it on",
        });
        if !item.is_loaded() {
            steps.push(Step::Launchctl {
                args: vec![
                    "bootstrap".into(),
                    domain,
                    plist.to_string_lossy().into_owned(),
                ],
                what: "Turned on for next login, but couldn't start it now",
            });
        }
    } else {
        steps.push(Step::Launchctl {
            args: vec!["disable".into(), target.clone()],
            what: "Couldn't turn it off",
        });
        if item.is_loaded() {
            steps.push(Step::Launchctl {
                args: vec!["bootout".into(), target],
                what: "Turned off for next login, but couldn't stop it now",
            });
        }
    }
    Ok(steps)
}

/// Steps to remove a user agent: stop it if loaded, then Trash the plist.
pub fn plan_remove(item: &LaunchItem, uid: u32) -> Result<Vec<Step>, String> {
    refuse(item)?;
    let mut steps = Vec::new();
    if item.is_loaded() {
        steps.push(Step::Launchctl {
            args: vec![
                "bootout".into(),
                format!("{}/{}", gui_domain(uid), item.label),
            ],
            what: "Couldn't stop it",
        });
    }
    steps.push(Step::Trash(item.plist.clone()));
    Ok(steps)
}

/// Run a plan in order, stopping at the first failure.
pub fn execute(steps: &[Step]) -> Result<(), String> {
    for step in steps {
        match step {
            Step::Launchctl { args, what } => {
                let out = Command::new("/bin/launchctl")
                    .args(args)
                    .stdin(Stdio::null())
                    .output()
                    .map_err(|e| format!("{what}: {e}"))?;
                if !out.status.success() {
                    let err = String::from_utf8_lossy(&out.stderr);
                    let fallback =
                        format!("launchctl exited with {}", out.status.code().unwrap_or(-1));
                    return Err(format!("{what}: {}", first_line(&err, &fallback)));
                }
            }
            Step::Rename { from, to } => {
                if to.exists() {
                    return Err(format!(
                        "Couldn't turn it on: {} already exists",
                        to.display()
                    ));
                }
                std::fs::rename(from, to).map_err(|e| format!("Couldn't turn it on: {e}"))?;
            }
            Step::Trash(path) => {
                nera_core::fs_util::trash_item(path)
                    .map_err(|e| format!("Couldn't move it to the Trash: {e}"))?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests (fixtures and temp dirs only; nothing here runs launchctl changes)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple Computer//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.amazon.codewhisperer.launcher</string>
    <key>ProgramArguments</key>
    <array>
        <string>/Applications/Kiro CLI.app/Contents/MacOS/kiro_cli_desktop</string>
        <string>--is-startup</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>AssociatedBundleIdentifiers</key>
    <array><string>com.amazon.codewhisperer</string></array>
</dict>
</plist>"#;

    const DAEMON: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>Label</key><string>us.zoom.ZoomDaemon</string>
    <key>Program</key><string>/Library/PrivilegedHelperTools/us.zoom.ZoomDaemon</string>
    <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
    <key>Disabled</key><true/>
</dict>
</plist>"#;

    const SCHEDULED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>Label</key><string>homebrew.mxcl.redis</string>
    <key>Program</key><string>/opt/homebrew/opt/redis/bin/redis-server</string>
    <key>StartInterval</key><integer>3600</integer>
    <key>KeepAlive</key><false/>
</dict>
</plist>"#;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nera-startup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn user_item(label: &str, loaded: Option<Loaded>) -> LaunchItem {
        LaunchItem {
            id: format!("/tmp/{label}.plist"),
            plist: PathBuf::from(format!("/tmp/{label}.plist")),
            scope: Scope::User,
            label: label.into(),
            info: Some(PlistInfo {
                label: Some(label.into()),
                ..Default::default()
            }),
            read_error: None,
            vendor: "Test".into(),
            is_apple: false,
            legacy_hidden: false,
            override_disabled: None,
            loaded: Some(loaded),
            program_missing: false,
        }
    }

    #[test]
    fn parses_agent_plist() {
        let info = parse_plist_bytes(AGENT.as_bytes()).unwrap();
        assert_eq!(
            info.label.as_deref(),
            Some("com.amazon.codewhisperer.launcher")
        );
        assert_eq!(
            info.executable(),
            Some("/Applications/Kiro CLI.app/Contents/MacOS/kiro_cli_desktop")
        );
        assert_eq!(info.arguments.len(), 2);
        assert!(info.run_at_load && !info.keep_alive && !info.disabled);
        assert_eq!(
            info.bundle_ids,
            vec!["com.amazon.codewhisperer".to_string()]
        );
        assert_eq!(info.triggers(), vec![Trigger::AtLogin]);
    }

    #[test]
    fn parses_keepalive_dict_disabled_and_schedule() {
        let d = parse_plist_bytes(DAEMON.as_bytes()).unwrap();
        assert_eq!(
            d.executable(),
            Some("/Library/PrivilegedHelperTools/us.zoom.ZoomDaemon")
        );
        // A KeepAlive dictionary is conditional: not "always on".
        assert!(!d.keep_alive && d.keep_alive_conditional && d.disabled);
        assert_eq!(d.triggers(), vec![Trigger::Conditional]);
        assert_eq!(Trigger::AtLogin.label(Scope::Daemon), "At startup");

        let s = parse_plist_bytes(SCHEDULED.as_bytes()).unwrap();
        assert!(s.scheduled && !s.keep_alive);
        assert_eq!(s.triggers(), vec![Trigger::Scheduled]);
    }

    #[test]
    fn combines_triggers_and_detects_watch_paths() {
        let both = r#"<plist version="1.0"><dict><key>Label</key><string>a.b</string><key>RunAtLoad</key><true/><key>StartInterval</key><integer>60</integer></dict></plist>"#;
        let info = parse_plist_bytes(both.as_bytes()).unwrap();
        assert_eq!(info.triggers(), vec![Trigger::AtLogin, Trigger::Scheduled]);
        let watch = r#"<plist version="1.0"><dict><key>Label</key><string>a.b</string><key>WatchPaths</key><array><string>/x</string></array><key>KeepAlive</key><false/></dict></plist>"#;
        assert_eq!(
            parse_plist_bytes(watch.as_bytes()).unwrap().triggers(),
            vec![Trigger::FileChange]
        );
        let always = r#"<plist version="1.0"><dict><key>Label</key><string>a.b</string><key>KeepAlive</key><true/><key>RunAtLoad</key><true/></dict></plist>"#;
        assert_eq!(
            parse_plist_bytes(always.as_bytes()).unwrap().triggers(),
            vec![Trigger::AlwaysOn]
        );
        let none = r#"<plist version="1.0"><dict><key>Label</key><string>a.b</string><key>MachServices</key><dict/></dict></plist>"#;
        assert_eq!(
            parse_plist_bytes(none.as_bytes()).unwrap().triggers(),
            vec![Trigger::OnDemand]
        );
        assert_eq!(
            Trigger::Conditional.label(Scope::User),
            "Restarts on conditions"
        );
    }

    #[test]
    fn rejects_invalid_plists() {
        assert!(parse_plist_bytes(b"not a plist").is_err());
        let array = r#"<?xml version="1.0"?><plist version="1.0"><array/></plist>"#;
        assert!(parse_plist_bytes(array.as_bytes()).is_err());
    }

    #[test]
    fn parses_binary_plists() {
        let mut dict = plist::Dictionary::new();
        dict.insert(
            "Label".into(),
            plist::Value::String("org.example.bin".into()),
        );
        dict.insert("KeepAlive".into(), plist::Value::Boolean(true));
        let mut buf = Vec::new();
        plist::Value::Dictionary(dict)
            .to_writer_binary(&mut buf)
            .unwrap();
        let info = parse_plist_bytes(&buf).unwrap();
        assert_eq!(info.label.as_deref(), Some("org.example.bin"));
        assert!(info.keep_alive);
    }

    #[test]
    fn derives_vendor() {
        let app = "/Applications/Kiro CLI.app/Contents/MacOS/kiro";
        assert_eq!(
            vendor("com.amazon.codewhisperer.launcher", Some(app), &[]),
            "Kiro CLI"
        );
        assert_eq!(
            vendor("homebrew.mxcl.redis", Some("/opt/homebrew/bin/redis"), &[]),
            "Homebrew"
        );
        assert_eq!(vendor("sh.brew.postgresql@16", None, &[]), "Homebrew");
        assert_eq!(
            vendor(
                "gitlab-runner",
                Some("/opt/homebrew/bin/gitlab-runner"),
                &[]
            ),
            "Homebrew"
        );
        assert_eq!(
            vendor("com.adobe.ccxprocess", Some("/Library/x"), &[]),
            "Adobe"
        );
        assert_eq!(
            vendor("com.rogueamoeba.arkaudiod", None, &[]),
            "Rogue Amoeba"
        );
        assert_eq!(vendor("us.zoom.ZoomDaemon", None, &[]), "Zoom");
        assert_eq!(
            vendor(
                "S8EX82NJP6.com.macpaw.CleanMyMac-mas.HealthMonitor",
                None,
                &[]
            ),
            "MacPaw"
        );
        assert_eq!(vendor("net.acme-corp.helper", None, &[]), "Acme-corp");
        assert_eq!(vendor("my-helper", None, &["io.fancy.app".into()]), "Fancy");
        assert_eq!(vendor("my-helper", Some("/usr/bin/true"), &[]), "Unknown");
    }

    #[test]
    fn detects_apple_items() {
        assert!(is_apple("com.apple.Siri.agent", None));
        assert!(is_apple("weird", Some("/System/Library/CoreServices/x")));
        assert!(!is_apple("com.applesauce.x", None));
        assert_eq!(
            app_name_in("/Applications/A.app/Contents/Helpers/B.app/Contents/MacOS/b").as_deref(),
            Some("A")
        );
        assert_eq!(app_name_in("/usr/bin/true"), None);
    }

    #[test]
    fn parses_launchctl_print_services() {
        let out = "gui/501 = {\n\ttype = login\n\tservices = {\n\t\t   20019      - \tcom.example.running\n\t\t       0      1 \tcom.example.idle\n\t\t       0      - \tlabel with space\n\t}\n\n\tendpoints = {\n\t\t\"x\" = {\n\t\t}\n\t}\n}\n";
        let map = parse_print_services(out);
        assert_eq!(map.len(), 3);
        assert_eq!(
            map["com.example.running"],
            Loaded {
                pid: Some(20019),
                last_exit: None
            }
        );
        assert_eq!(
            map["com.example.idle"],
            Loaded {
                pid: None,
                last_exit: Some(1)
            }
        );
        assert!(map.contains_key("label with space"));
    }

    #[test]
    fn parses_launchctl_list_and_overrides() {
        let list = "PID\tStatus\tLabel\n-\t0\tcom.apple.SafariHistoryServiceAgent\n14392\t0\tcom.apple.progressd\n-\t1\tcom.nextera.agent\n";
        let map = parse_list(list);
        assert_eq!(map["com.apple.progressd"].pid, Some(14392));
        assert_eq!(
            map["com.nextera.agent"],
            Loaded {
                pid: None,
                last_exit: Some(1)
            }
        );

        let disabled = "\tdisabled services = {\n\t\t\"com.docker.helper\" => enabled\n\t\t\"com.apple.ftpd\" => disabled\n\t\t\"old.style\" => true\n\t}\n";
        let o = parse_print_disabled(disabled);
        assert_eq!(o.get("com.docker.helper"), Some(&false));
        assert_eq!(o.get("com.apple.ftpd"), Some(&true));
        assert_eq!(o.get("old.style"), Some(&true));
        assert_eq!(o.len(), 3);
    }

    #[test]
    fn effective_state_and_status() {
        let mut item = user_item("a.b.c", None);
        assert!(item.enabled());
        assert_eq!(item.status(), Status::NotLoaded);
        item.info.as_mut().unwrap().disabled = true;
        assert!(!item.enabled());
        assert_eq!(item.status(), Status::Disabled);
        item.override_disabled = Some(false); // override wins over plist key
        assert!(item.enabled());
        item.loaded = Some(Some(Loaded {
            pid: Some(5),
            last_exit: None,
        }));
        assert_eq!(item.status(), Status::Running);
        item.loaded = Some(Some(Loaded {
            pid: None,
            last_exit: Some(0),
        }));
        assert_eq!(item.status(), Status::Loaded);
        item.loaded = None;
        item.override_disabled = None;
        item.info.as_mut().unwrap().disabled = false;
        assert_eq!(item.status(), Status::Unknown);
        item.legacy_hidden = true;
        assert!(!item.enabled());
    }

    #[test]
    fn reads_a_folder_of_fixtures() {
        let dir = temp_dir("folder");
        std::fs::write(dir.join("com.amazon.codewhisperer.launcher.plist"), AGENT).unwrap();
        std::fs::write(dir.join(".homebrew.mxcl.redis.plist"), SCHEDULED).unwrap();
        std::fs::write(dir.join("broken.plist"), "garbage").unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let missing = r#"<?xml version="1.0"?><plist version="1.0"><dict><key>Label</key><string>com.gone.app</string><key>Program</key><string>/nonexistent/nera/gone</string></dict></plist>"#;
        std::fs::write(dir.join("com.gone.app.plist"), missing).unwrap();

        let mut items = read_folder(&dir, Scope::User).unwrap();
        sort_items(&mut items);
        assert_eq!(items.len(), 4);
        let by = |label: &str| items.iter().find(|i| i.label == label).unwrap();
        let kiro = by("com.amazon.codewhisperer.launcher");
        assert_eq!(kiro.vendor, "Kiro CLI");
        assert!(!kiro.legacy_hidden && kiro.read_error.is_none());
        let redis = by("homebrew.mxcl.redis");
        assert!(redis.legacy_hidden && !redis.enabled());
        let broken = by("broken");
        assert!(broken.read_error.is_some() && broken.info.is_none() && !broken.is_editable());
        assert!(by("com.gone.app").program_missing);
        assert!(!kiro.program_missing || !Path::new("/Applications/Kiro CLI.app").exists());

        // Hidden files are only meaningful in the user's folder.
        assert_eq!(read_folder(&dir, Scope::Daemon).unwrap().len(), 3);
        assert!(read_folder(&dir.join("missing"), Scope::User)
            .unwrap()
            .is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn plist_without_label_is_flagged_and_never_matched_to_launchd() {
        let dir = temp_dir("nolabel");
        std::fs::write(
            dir.join("com.google.keystone.agent.plist"),
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict/></plist>",
        )
        .unwrap();
        let mut items = read_folder(&dir, Scope::User).unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].has_no_label());
        assert!(items[0]
            .subtitle()
            .ends_with("No Label key, launchd ignores this file"));
        let mut snap = Snapshot {
            items: std::mem::take(&mut items),
            ..Default::default()
        };
        // Another job with the same name as the file is loaded; it is not this one.
        let loaded = HashMap::from([(
            "com.google.keystone.agent".to_string(),
            Loaded {
                pid: Some(7),
                last_exit: None,
            },
        )]);
        let state = LaunchdState {
            gui: Ok(loaded),
            system: Ok(HashMap::new()),
            gui_overrides: Ok(HashMap::from([(
                "com.google.keystone.agent".to_string(),
                true,
            )])),
            system_overrides: Ok(HashMap::new()),
        };
        apply_state(&mut snap, &state);
        assert_eq!(snap.items[0].status(), Status::NotLoaded);
        assert!(snap.items[0].enabled());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unreadable_launchd_state_is_reported_not_assumed() {
        let mut item = user_item("com.x.agent", None);
        item.loaded = None;
        let mut snap = Snapshot {
            items: vec![item],
            ..Default::default()
        };
        let state = LaunchdState {
            gui: Ok(HashMap::new()),
            system: Ok(HashMap::new()),
            gui_overrides: Err("print-disabled failed".into()),
            system_overrides: Ok(HashMap::new()),
        };
        apply_state(&mut snap, &state);
        assert_eq!(snap.launchd_error.as_deref(), Some("print-disabled failed"));
    }

    #[test]
    fn only_a_truly_missing_path_counts_as_missing() {
        assert!(path_is_missing(Path::new("/nonexistent/nera/gone")));
        assert!(!path_is_missing(Path::new("/bin/ls")));
    }

    #[test]
    fn plans_toggle_for_user_agents_only() {
        let off = plan_toggle(
            &user_item(
                "com.x.agent",
                Some(Loaded {
                    pid: Some(9),
                    last_exit: None,
                }),
            ),
            false,
            501,
        )
        .unwrap();
        assert_eq!(
            off,
            vec![
                Step::Launchctl {
                    args: vec!["disable".into(), "gui/501/com.x.agent".into()],
                    what: "Couldn't turn it off"
                },
                Step::Launchctl {
                    args: vec!["bootout".into(), "gui/501/com.x.agent".into()],
                    what: "Turned off for next login, but couldn't stop it now"
                },
            ]
        );
        // Not loaded: nothing to stop.
        assert_eq!(
            plan_toggle(&user_item("com.x.agent", None), false, 501)
                .unwrap()
                .len(),
            1
        );

        let on = plan_toggle(&user_item("com.x.agent", None), true, 501).unwrap();
        assert!(
            matches!(&on[0], Step::Launchctl { args, .. } if args == &["enable", "gui/501/com.x.agent"])
        );
        assert!(
            matches!(&on[1], Step::Launchctl { args, .. } if args == &["bootstrap", "gui/501", "/tmp/com.x.agent.plist"])
        );

        let mut system = user_item("com.vendor.daemon", None);
        system.scope = Scope::Daemon;
        assert!(plan_toggle(&system, false, 501).is_err());
        let mut apple = user_item("com.apple.thing", None);
        apple.is_apple = true;
        assert!(plan_toggle(&apple, false, 501).is_err());
        assert!(plan_remove(&apple, 501).is_err());
    }

    #[test]
    fn plans_enable_of_legacy_hidden_agent() {
        let mut item = user_item("com.x.old", None);
        item.plist = PathBuf::from("/tmp/la/.com.x.old.plist");
        item.legacy_hidden = true;
        let steps = plan_toggle(&item, true, 501).unwrap();
        assert_eq!(
            steps[0],
            Step::Rename {
                from: "/tmp/la/.com.x.old.plist".into(),
                to: "/tmp/la/com.x.old.plist".into()
            }
        );
        assert!(
            matches!(&steps[2], Step::Launchctl { args, .. } if args[2] == "/tmp/la/com.x.old.plist")
        );
    }

    #[test]
    fn plans_remove() {
        let loaded = user_item(
            "com.x.agent",
            Some(Loaded {
                pid: None,
                last_exit: Some(0),
            }),
        );
        let steps = plan_remove(&loaded, 501).unwrap();
        assert!(
            matches!(&steps[0], Step::Launchctl { args, .. } if args == &["bootout", "gui/501/com.x.agent"])
        );
        assert_eq!(steps[1], Step::Trash("/tmp/com.x.agent.plist".into()));
        assert_eq!(
            plan_remove(&user_item("com.x.agent", None), 501)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn executes_rename_and_trash_on_temp_files_only() {
        let dir = temp_dir("exec");
        let hidden = dir.join(".com.x.old.plist");
        let visible = dir.join("com.x.old.plist");
        std::fs::write(&hidden, AGENT).unwrap();
        execute(&[Step::Rename {
            from: hidden.clone(),
            to: visible.clone(),
        }])
        .unwrap();
        assert!(visible.exists() && !hidden.exists());
        // Refuses to overwrite.
        std::fs::write(&hidden, AGENT).unwrap();
        assert!(execute(&[Step::Rename {
            from: hidden.clone(),
            to: visible.clone()
        }])
        .is_err());
        // Protected paths are refused by the foundation's safety checks.
        assert!(execute(&[Step::Trash(PathBuf::from("/System"))]).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn parses_login_items_output() {
        let out = "Slack\t/Applications/Slack.app\tfalse\nBartender 5\t/Applications/Bartender 5.app\ttrue\n\nGhost\tmissing value\tfalse\nSlack\t/Applications/Slack.app\tfalse\n";
        let items = parse_login_items(out);
        assert_eq!(items.len(), 3);
        assert_eq!(
            items[0],
            LoginItem {
                name: "Bartender 5".into(),
                path: "/Applications/Bartender 5.app".into(),
                hidden: true
            }
        );
        assert_eq!(items[1].path, "");
        assert_eq!(items[1].id(), "login-Ghost");
        assert_eq!(items[2].id(), "login-/Applications/Slack.app");
        assert!(parse_login_items("").is_empty());
    }

    #[test]
    fn classifies_osascript_errors() {
        let denied =
            "execution error: Not authorized to send Apple events to System Events. (-1743)";
        assert_eq!(classify_osascript_error(denied), LoginError::NotAuthorized);
        assert_eq!(
            classify_osascript_error("\n0:5: syntax error: boom (-2741)\n"),
            LoginError::Failed("0:5: syntax error: boom (-2741)".into())
        );
    }

    /// Read-only check against this Mac (launchctl print, plist reads).
    #[test]
    #[ignore]
    fn scans_real_system_read_only() {
        let snap = scan();
        for i in &snap.items {
            eprintln!(
                "{:?} {} | {} | {:?} | {:?} | {:?}",
                i.scope,
                i.label,
                i.vendor,
                i.status(),
                i.trigger_label(),
                i.read_error
            );
        }
        eprintln!(
            "folder errors: {:?} launchd: {:?}",
            snap.folder_errors, snap.launchd_error
        );
        assert!(snap.launchd_error.is_none());
    }

    #[test]
    fn subtitle_and_visibility() {
        let mut a = user_item("com.x.agent", None);
        a.vendor = "Acme".into();
        a.info.as_mut().unwrap().program = Some("/opt/acme/bin/helper".into());
        a.info.as_mut().unwrap().run_at_load = true;
        assert_eq!(
            a.subtitle(),
            "Acme \u{b7} At login \u{b7} /opt/acme/bin/helper"
        );
        a.read_error = Some("Only an administrator can read this file".into());
        assert!(a.subtitle().ends_with("administrator can read this file"));

        let mut apple = user_item("com.apple.thing", None);
        apple.is_apple = true;
        let items = [a.clone(), apple];
        assert_eq!(visible(items.iter(), "", false).len(), 1);
        assert_eq!(visible(items.iter(), "", true).len(), 2);
        assert_eq!(visible(items.iter(), "thing", true).len(), 1);
        assert_eq!(visible(items.iter(), "thing", false).len(), 0);
        let home = nera_core::paths::home();
        assert_eq!(
            tilde(&format!("{}/Library/x", home.display())),
            "~/Library/x"
        );
        assert_eq!(tilde("/opt/x"), "/opt/x");
    }

    #[test]
    fn search_matches_label_vendor_and_program() {
        let mut item = user_item("com.x.agent", None);
        item.vendor = "Acme".into();
        item.info.as_mut().unwrap().program = Some("/opt/acme/bin/helper".into());
        assert!(item.matches("ACME"));
        assert!(item.matches("helper"));
        assert!(item.matches("x.agent"));
        assert!(item.matches("  "));
        assert!(!item.matches("zoom"));
    }
}
