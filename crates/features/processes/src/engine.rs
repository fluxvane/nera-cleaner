//! Processes engine: live process sampling, filtering/sorting, the quit
//! policy and sending quit signals. No UI; unit-tested.
//!
//! Every figure follows Activity Monitor's definition:
//! * CPU is the delta of a process's user + system time between two samples
//!   (`proc_pid_rusage`), in % of one core, so a busy multi-threaded process
//!   can exceed 100 %. A process first seen in this sample has no CPU figure.
//! * Memory is the physical footprint (`ri_phys_footprint`), not the resident
//!   size, in binary units (1 GB = 1024^3 bytes).
//! * System memory used is App Memory + Wired + Compressed.
//! * Apps are what Launch Services registers as regular (Dock) apps.
//!
//! macOS shares CPU and memory counters only for the user's own processes;
//! for the rest those figures are `None` and the screen shows a dash.
//!
//! The sampler, the quit policy and quitting are per platform: the macOS ones
//! are below, the Windows ones (Task Manager's definitions) in
//! `engine_windows.rs`. Everything else is shared.

#[cfg(target_os = "macos")]
use std::collections::{HashMap, HashSet};
#[cfg(target_os = "macos")]
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
#[cfg(target_os = "macos")]
use std::time::Instant;

#[cfg(target_os = "macos")]
use sysinfo::{MemoryRefreshKind, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[cfg(target_os = "macos")]
use crate::registry::{self, AppKind, RegisteredApp};
#[cfg(target_os = "macos")]
use crate::sys;

#[cfg(windows)]
#[path = "engine_windows.rs"]
mod windows;
#[cfg(windows)]
pub use self::windows::{control, matches_query, quit, subtitle, Sampler};

/// Title of the system memory tile: Activity Monitor's "Memory Used" on
/// macOS, Task Manager's "In use" on Windows.
#[cfg(target_os = "macos")]
pub const MEMORY_TILE: &str = "Memory used";
#[cfg(windows)]
pub const MEMORY_TILE: &str = "Memory in use";

/// What `Snapshot::cores` counts, as the platform's own tools word it.
#[cfg(target_os = "macos")]
const CORE_NOUN: &str = "core";
#[cfg(windows)]
const CORE_NOUN: &str = "logical processor";

/// One running process at the time of a sample.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessInfo {
    pub pid: i32,
    pub parent: Option<i32>,
    /// What the user sees: the app name for apps, otherwise the process name.
    pub name: String,
    /// Effective user id of the owner (what `ps -o user` shows), when known.
    pub uid: Option<u32>,
    /// Owner's short user name (or the numeric id when unknown).
    pub user: String,
    /// CPU use in % of one core over the last interval; `None` when macOS
    /// doesn't share it or the process is too new to have an interval.
    pub cpu: Option<f32>,
    /// Memory footprint in bytes (Activity Monitor's "Memory" column);
    /// `None` when macOS doesn't share it.
    pub memory: Option<u64>,
    pub exe: Option<PathBuf>,
    /// Launch Services registration, when the process is a registered app.
    pub bundle: Option<BundleInfo>,
    /// Seconds since the Unix epoch; together with `pid` this identifies the process.
    pub start_time: u64,
    pub run_time: u64,
}

impl ProcessInfo {
    /// A regular (Dock-visible) app, as opposed to agents, helpers and daemons.
    pub fn is_app(&self) -> bool {
        self.bundle.as_ref().is_some_and(|b| !b.background)
    }

    /// macOS only shares CPU and memory figures for the user's own processes.
    pub fn has_details(&self) -> bool {
        self.memory.is_some()
    }

    /// Stable domain id for list rows: a pid can be reused after a process
    /// exits, the (pid, start time) pair cannot.
    pub fn key(&self) -> String {
        format!("proc-{}-{}", self.pid, self.start_time)
    }
}

/// Facts about the app a process belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleInfo {
    pub path: PathBuf,
    /// Display name as the Dock and Activity Monitor show it.
    pub name: String,
    pub bundle_id: Option<String>,
    /// Registered as a menu-bar extra or faceless app rather than a regular app.
    pub background: bool,
}

/// Whole-system figures plus the process list.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub processes: Vec<ProcessInfo>,
    /// Average load across all cores, 0-100.
    pub cpu_load: f32,
    pub cores: usize,
    /// Activity Monitor's "Memory Used"; `None` when the kernel's page
    /// counters can't be read (no other definition is substituted).
    pub memory_used: Option<u64>,
    pub memory_total: u64,
    /// False when Launch Services couldn't be read, so which processes are
    /// apps is unknown (the screen then shows no app count).
    pub apps_known: bool,
}

impl Snapshot {
    pub fn find(&self, pid: i32) -> Option<&ProcessInfo> {
        self.processes.iter().find(|p| p.pid == pid)
    }
}

/// The running user and app, used by the quit policy.
#[derive(Clone, Debug)]
pub struct Me {
    pub uid: u32,
    pub pid: i32,
    pub exe: Option<PathBuf>,
}

#[cfg(target_os = "macos")]
impl Me {
    pub fn current() -> Self {
        // SAFETY: geteuid has no preconditions and cannot fail.
        let uid = unsafe { libc::geteuid() };
        Self {
            uid,
            pid: std::process::id() as i32,
            exe: std::env::current_exe()
                .ok()
                .map(|p| std::fs::canonicalize(&p).unwrap_or(p)),
        }
    }
}

// MARK: - Sampling

/// CPU counters of the previous sample, per pid.
#[cfg(target_os = "macos")]
struct Prev {
    start_time: u64,
    cpu_ns: u64,
    at: Instant,
}

/// Cached Launch Services registry.
#[cfg(target_os = "macos")]
struct Registry {
    apps: Option<HashMap<i32, RegisteredApp>>,
    at: Option<Instant>,
    pids: HashSet<i32>,
}

/// Re-read the registry at least this often (also retried this often if it fails).
#[cfg(target_os = "macos")]
const REGISTRY_TTL: Duration = Duration::from_secs(15);

/// Long-lived sampler. Keep one per screen and call [`Sampler::sample`] from
/// a background thread; the previous counters it keeps make CPU deltas right.
#[cfg(target_os = "macos")]
pub struct Sampler {
    sys: System,
    user_names: HashMap<u32, String>,
    prev: HashMap<i32, Prev>,
    registry: Registry,
    last: Option<Instant>,
}

/// Samples further apart than this are not trusted for CPU deltas (the
/// screen was hidden or paused), so the sampler takes a fresh pair.
const STALE_AFTER: Duration = Duration::from_secs(5);

/// CPU is measured over at least this long a window (what `top` uses), so a
/// manual refresh right after a sample can't report a few-millisecond blip.
const MIN_WINDOW: Duration = Duration::from_secs(1);

#[cfg(target_os = "macos")]
impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "macos")]
impl Sampler {
    pub fn new() -> Self {
        Self {
            sys: System::new(),
            user_names: HashMap::new(),
            prev: HashMap::new(),
            registry: Registry {
                apps: None,
                at: None,
                pids: HashSet::new(),
            },
            last: None,
        }
    }

    /// Refresh everything and return a snapshot. Blocks briefly on the first
    /// call (and after a long pause) to measure CPU over a short window.
    pub fn sample(&mut self) -> anyhow::Result<Snapshot> {
        let stale = self.last.is_none_or(|t| t.elapsed() > STALE_AFTER);
        if stale {
            self.refresh();
            self.measure();
            std::thread::sleep(MIN_WINDOW);
        } else if let Some(wait) = self.last.and_then(|t| MIN_WINDOW.checked_sub(t.elapsed())) {
            std::thread::sleep(wait);
        }
        self.refresh();
        let usage = self.measure();
        self.last = Some(Instant::now());
        self.refresh_registry();

        let mut processes = Vec::with_capacity(self.sys.processes().len());
        let pids: Vec<_> = self.sys.processes().keys().copied().collect();
        for pid in pids {
            let Some(p) = self.sys.process(pid) else {
                continue;
            };
            let pid = pid.as_u32() as i32;
            if pid < 0 {
                continue;
            }
            let exe = p.exe().map(Path::to_path_buf);
            let fallback_name = p.name().to_string_lossy().into_owned();
            let fallback_uid = p.effective_user_id().map(|u| **u);
            let fallback_parent = p.parent().map(|pp| pp.as_u32() as i32);
            let (start_time, run_time) = (p.start_time(), p.run_time());
            let short = sys::short_info(pid);
            if short.is_some_and(|s| s.zombie) {
                continue;
            }
            let uid = short.map(|s| s.uid).or(fallback_uid);
            let parent = short.map(|s| s.ppid).or(fallback_parent);
            let bundle = self.bundle_for(pid, exe.as_deref());
            let kernel_name = sys::proc_name(pid);
            let name = display_name(
                kernel_name.as_deref().unwrap_or(&fallback_name),
                exe.as_deref(),
                bundle.as_ref(),
            );
            if name.is_empty() {
                continue;
            }
            let user = uid
                .map(|u| self.user_name(u))
                .unwrap_or_else(|| "unknown".into());
            let (cpu, memory) = usage.get(&pid).copied().unwrap_or((None, None));
            processes.push(ProcessInfo {
                pid,
                parent,
                name,
                uid,
                user,
                cpu: cpu.filter(|c| c.is_finite()).map(|c| c.max(0.0)),
                memory,
                exe,
                bundle,
                start_time,
                run_time,
            });
        }
        // macOS lists the kernel (pid 0) but sysinfo can't read it; Activity
        // Monitor and `top` show it as `kernel_task`, so the count matches theirs.
        if !processes.iter().any(|p| p.pid == 0) && sys::short_info(0).is_some_and(|s| !s.zombie) {
            processes.push(ProcessInfo {
                pid: 0,
                parent: Some(0),
                name: "kernel_task".into(),
                uid: Some(0),
                user: self.user_name(0),
                cpu: None,
                memory: None,
                exe: None,
                bundle: None,
                start_time: 0,
                run_time: 0,
            });
        }
        if processes.is_empty() {
            anyhow::bail!("macOS returned an empty process list");
        }

        Ok(Snapshot {
            processes,
            cpu_load: self.sys.global_cpu_usage().clamp(0.0, 100.0),
            cores: self.sys.cpus().len().max(1),
            memory_used: sys::memory_used(),
            memory_total: self.sys.total_memory(),
            apps_known: self.registry.apps.is_some(),
        })
    }

    fn refresh(&mut self) {
        self.sys.refresh_cpu_usage();
        self.sys
            .refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_exe(UpdateKind::OnlyIfNotSet)
                .with_user(UpdateKind::OnlyIfNotSet),
        );
    }

    /// Read every listed process's counters and turn the change since the
    /// previous call into CPU %. Returns (cpu, footprint) per pid.
    fn measure(&mut self) -> HashMap<i32, (Option<f32>, Option<u64>)> {
        let mut out = HashMap::with_capacity(self.sys.processes().len());
        let mut next = HashMap::with_capacity(self.sys.processes().len());
        for (pid, p) in self.sys.processes() {
            let pid = pid.as_u32() as i32;
            let Some(u) = sys::usage(pid) else {
                out.insert(pid, (None, None));
                continue;
            };
            let at = Instant::now();
            let start_time = p.start_time();
            let cpu = self
                .prev
                .get(&pid)
                .filter(|prev| prev.start_time == start_time && u.cpu_ns >= prev.cpu_ns)
                .map(|prev| {
                    sys::cpu_percent(
                        u.cpu_ns - prev.cpu_ns,
                        at.duration_since(prev.at).as_nanos() as u64,
                    )
                });
            out.insert(pid, (cpu, Some(u.footprint)));
            next.insert(
                pid,
                Prev {
                    start_time,
                    cpu_ns: u.cpu_ns,
                    at,
                },
            );
        }
        self.prev = next;
        out
    }

    /// Re-query Launch Services when it is stale or a new app-bundle process appeared.
    fn refresh_registry(&mut self) {
        let current: HashSet<i32> = self
            .sys
            .processes()
            .keys()
            .map(|p| p.as_u32() as i32)
            .collect();
        let in_bundle = |pid: i32| {
            self.sys
                .process(sysinfo::Pid::from_u32(pid as u32))
                .and_then(|p| p.exe())
                .is_some_and(|e| e.to_string_lossy().contains(".app/"))
        };
        let new_bundle_process = current
            .iter()
            .any(|pid| !self.registry.pids.contains(pid) && in_bundle(*pid));
        let expired = self.registry.at.is_none_or(|t| t.elapsed() > REGISTRY_TTL);
        if expired || new_bundle_process {
            self.registry.apps = registry::query();
            self.registry.at = Some(Instant::now());
            self.registry.pids = current;
        }
    }

    /// Short name of a user id, as `ps` shows it (system accounts such as
    /// `_locationd` included); the number itself when the account is unknown.
    fn user_name(&mut self, uid: u32) -> String {
        self.user_names
            .entry(uid)
            .or_insert_with(|| sys::user_name(uid).unwrap_or_else(|| uid.to_string()))
            .clone()
    }

    /// The registered app this process is the main process of, if any. A pid
    /// is only trusted when its executable lies inside the registered bundle
    /// (guards against pid reuse between registry reads).
    fn bundle_for(&self, pid: i32, exe: Option<&Path>) -> Option<BundleInfo> {
        let app = self.registry.apps.as_ref()?.get(&pid)?;
        let path = app.bundle_path.clone()?;
        if !exe.is_some_and(|e| e.starts_with(&path)) {
            return None;
        }
        Some(BundleInfo {
            path,
            name: app.name.clone(),
            bundle_id: app.bundle_id.clone(),
            background: app.kind != AppKind::Foreground,
        })
    }
}

/// The name to show. Regular apps use their registered display name (what the
/// Dock shows); everything else uses the kernel's process name, replaced by
/// the executable's file name when the kernel truncated it.
#[cfg(target_os = "macos")]
fn display_name(kernel: &str, exe: Option<&Path>, bundle: Option<&BundleInfo>) -> String {
    if let Some(b) = bundle.filter(|b| !b.background && !b.name.is_empty()) {
        return b.name.clone();
    }
    let kernel = kernel.trim();
    if let Some(file) = exe.and_then(|e| e.file_name()).map(|f| f.to_string_lossy()) {
        if file.len() > kernel.len() && file.starts_with(kernel) {
            return file.into_owned();
        }
    }
    kernel.to_string()
}

// MARK: - Filtering and sorting

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    Apps,
    Mine,
    All,
}

impl Filter {
    pub const ALL: [Filter; 3] = [Filter::Apps, Filter::Mine, Filter::All];

    pub fn label(self) -> &'static str {
        match self {
            Filter::Apps => "Apps",
            Filter::Mine => "My processes",
            Filter::All => "All",
        }
    }

    pub fn matches(self, p: &ProcessInfo, my_uid: u32) -> bool {
        match self {
            Filter::Apps => p.is_app() && p.uid == Some(my_uid),
            Filter::Mine => p.uid == Some(my_uid),
            Filter::All => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SortKey {
    #[default]
    Cpu,
    Memory,
    Name,
}

impl SortKey {
    pub const ALL: [SortKey; 3] = [SortKey::Cpu, SortKey::Memory, SortKey::Name];

    pub fn label(self) -> &'static str {
        match self {
            SortKey::Cpu => "CPU",
            SortKey::Memory => "Memory",
            SortKey::Name => "Name",
        }
    }
}

/// True when `query` (case-insensitive) matches the name, owner, pid or app path.
#[cfg(target_os = "macos")]
pub fn matches_query(p: &ProcessInfo, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    p.name.to_lowercase().contains(&q)
        || p.user.to_lowercase() == q
        || p.pid.to_string() == q
        || p.bundle
            .as_ref()
            .and_then(|b| b.bundle_id.as_deref())
            .is_some_and(|id| id.to_lowercase().contains(&q))
}

/// Filter, search and sort. Ties fall back to name then pid so rows don't
/// jump around between samples.
pub fn visible<'a>(
    list: &'a [ProcessInfo],
    filter: Filter,
    sort: SortKey,
    query: &str,
    my_uid: u32,
) -> Vec<&'a ProcessInfo> {
    let mut out: Vec<&ProcessInfo> = list
        .iter()
        .filter(|p| filter.matches(p, my_uid) && matches_query(p, query))
        .collect();
    let by_name = |a: &ProcessInfo, b: &ProcessInfo| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.pid.cmp(&b.pid))
    };
    match sort {
        // Unknown figures (None) sort last when descending.
        SortKey::Cpu => out.sort_by(|a, b| {
            cpu_key(b)
                .total_cmp(&cpu_key(a))
                .then_with(|| by_name(a, b))
        }),
        SortKey::Memory => out.sort_by(|a, b| b.memory.cmp(&a.memory).then_with(|| by_name(a, b))),
        SortKey::Name => out.sort_by(|a, b| by_name(a, b)),
    }
    out
}

fn cpu_key(p: &ProcessInfo) -> f32 {
    p.cpu.unwrap_or(-1.0)
}

/// "12.3%" — one decimal, like Activity Monitor.
pub fn format_cpu(cpu: f32) -> String {
    format!("{:.1}%", cpu.max(0.0))
}

/// Memory sizes as Activity Monitor shows them: binary units (1 GB = 1024^3
/// bytes, so a 64 GB Mac reads "64.00 GB"). `nera_core::format::bytes` is
/// decimal (Finder) and is right for files, not for RAM.
pub fn format_memory(bytes: u64) -> String {
    nera_core::format::memory_bytes(bytes)
}

/// Total CPU load, 0-100, no decimals.
pub fn format_cpu_load(load: f32) -> String {
    format!("{:.0}%", load.clamp(0.0, 100.0))
}

/// What a row's CPU cell shows: the figure, or a dash when macOS doesn't share it.
pub fn cpu_cell(p: &ProcessInfo) -> String {
    p.cpu.map_or_else(|| "\u{2014}".to_string(), format_cpu)
}

/// What a row's memory cell shows: the footprint, or a dash when unknown.
pub fn memory_cell(p: &ProcessInfo) -> String {
    p.memory
        .map_or_else(|| "\u{2014}".to_string(), format_memory)
}

/// The row's second line: "PID 123 · user", marking registered background apps.
#[cfg(target_os = "macos")]
pub fn subtitle(p: &ProcessInfo) -> String {
    let mut s = format!("PID {} \u{b7} {}", p.pid, p.user);
    if p.bundle.as_ref().is_some_and(|b| b.background) {
        s.push_str(" \u{b7} Background app");
    }
    s
}

/// Texts of the three summary tiles.
#[derive(Clone, Debug, PartialEq)]
pub struct StatsText {
    pub cpu_value: String,
    pub cpu_caption: String,
    pub cpu_fraction: f64,
    pub memory_value: String,
    pub memory_caption: String,
    pub memory_fraction: f64,
    pub processes_value: String,
    pub processes_caption: String,
}

pub fn stats_text(snap: &Snapshot, my_uid: u32) -> StatsText {
    let memory_fraction = match snap.memory_used {
        Some(used) if snap.memory_total > 0 => {
            (used as f64 / snap.memory_total as f64).clamp(0.0, 1.0)
        }
        _ => 0.0,
    };
    let mine = snap
        .processes
        .iter()
        .filter(|p| p.uid == Some(my_uid))
        .count();
    let apps = snap
        .processes
        .iter()
        .filter(|p| Filter::Apps.matches(p, my_uid))
        .count();
    let processes_caption = if snap.apps_known {
        format!("{apps} apps \u{b7} {mine} yours")
    } else {
        format!("{mine} yours")
    };
    StatsText {
        cpu_value: format_cpu_load(snap.cpu_load),
        cpu_caption: format!("Across {}", nera_core::format::count(snap.cores, CORE_NOUN)),
        cpu_fraction: f64::from(snap.cpu_load.clamp(0.0, 100.0)) / 100.0,
        memory_value: snap
            .memory_used
            .map_or_else(|| "\u{2014}".to_string(), format_memory),
        memory_caption: if snap.memory_used.is_some() {
            format!(
                "of {} \u{b7} {:.0}% used",
                format_memory(snap.memory_total),
                memory_fraction * 100.0
            )
        } else {
            format!("of {}", format_memory(snap.memory_total))
        },
        memory_fraction,
        processes_value: snap.processes.len().to_string(),
        processes_caption,
    }
}

// MARK: - Quit policy

/// Whether Nera lets the user quit a process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Control {
    Allowed,
    /// Allowed, but the confirmation explains the side effect.
    Caution(&'static str),
    /// Never offered. `badge` is a short label, `reason` a sentence.
    Protected {
        badge: &'static str,
        reason: &'static str,
    },
}

impl Control {
    pub fn can_quit(&self) -> bool {
        !matches!(self, Control::Protected { .. })
    }
}

/// Processes macOS can't run without (or that would log the user out).
#[cfg(target_os = "macos")]
const CRITICAL: &[&str] = &[
    "kernel_task",
    "launchd",
    "WindowServer",
    "loginwindow",
    "logind",
    "opendirectoryd",
    "securityd",
    "secd",
    "trustd",
    "tccd",
    "cfprefsd",
    "distnoted",
    "coreservicesd",
    "runningboardd",
    "UserEventAgent",
    "lsd",
    "launchservicesd",
    "mds",
    "mds_stores",
    "mdworker",
    "configd",
    "notifyd",
    "powerd",
    "coreaudiod",
    "hidd",
    "sysmond",
    "syslogd",
    "fseventsd",
];

/// User-facing system apps that macOS relaunches right away.
#[cfg(target_os = "macos")]
const RELAUNCHED: &[&str] = &[
    "Finder",
    "Dock",
    "SystemUIServer",
    "ControlCenter",
    "NotificationCenter",
    "Spotlight",
];

/// Where macOS keeps its own daemons and agents.
#[cfg(target_os = "macos")]
fn is_system_path(exe: &Path) -> bool {
    let s = exe.to_string_lossy();
    s.contains("/System/Library/")
        || ["/usr/libexec/", "/usr/sbin/", "/sbin/", "/Library/Apple/"]
            .iter()
            .any(|p| s.starts_with(p))
}

/// The quit policy: only the current user's own, non-critical processes,
/// never pid 0/1, root or other users' processes, and never Nera itself.
#[cfg(target_os = "macos")]
pub fn control(p: &ProcessInfo, me: &Me) -> Control {
    const SYSTEM: Control = Control::Protected {
        badge: "System",
        reason: "macOS needs this process to keep running.",
    };
    if p.pid <= 1 {
        return SYSTEM;
    }
    let is_me = p.pid == me.pid || (me.exe.is_some() && p.exe.as_ref() == me.exe.as_ref());
    if is_me {
        return Control::Protected {
            badge: "Nera",
            reason: "This is Nera Cleaner.",
        };
    }
    match p.uid {
        None | Some(0) => return SYSTEM,
        Some(uid) if uid != me.uid => {
            return Control::Protected {
                badge: "Other user",
                reason: "Owned by another user or a system account.",
            }
        }
        _ => {}
    }
    let base = p
        .exe
        .as_ref()
        .and_then(|e| e.file_name())
        .map(|f| f.to_string_lossy().into_owned());
    let named = |list: &[&str]| {
        list.iter()
            .any(|n| *n == p.name || base.as_deref() == Some(*n))
    };
    if named(CRITICAL) {
        return SYSTEM;
    }
    if named(RELAUNCHED) {
        return Control::Caution("macOS will relaunch it right away.");
    }
    if p.exe.as_deref().is_some_and(is_system_path) {
        return SYSTEM;
    }
    Control::Allowed
}

// MARK: - Quitting

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuitMode {
    /// SIGTERM: ask the process to exit.
    Quit,
    /// SIGKILL: stop it immediately.
    Force,
}

impl QuitMode {
    #[cfg(target_os = "macos")]
    pub fn signal(self) -> libc::c_int {
        match self {
            QuitMode::Quit => libc::SIGTERM,
            QuitMode::Force => libc::SIGKILL,
        }
    }

    pub fn verb(self) -> &'static str {
        match self {
            QuitMode::Quit => "Quit",
            QuitMode::Force => "Force quit",
        }
    }

    fn wait(self) -> Duration {
        match self {
            QuitMode::Quit => Duration::from_secs(3),
            QuitMode::Force => Duration::from_millis(1500),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuitOutcome {
    Exited,
    /// Signal delivered but the process is still alive after the wait.
    StillRunning,
    /// It had already exited (or its pid now belongs to another process).
    AlreadyGone,
    Refused(&'static str),
    Failed(String),
}

/// Liveness of the exact process (pid + start time), owner included.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Identity {
    Same { uid: u32 },
    Gone,
    Unknown,
}

#[cfg(target_os = "macos")]
fn identity(pid: i32, start_time: u64) -> Identity {
    // SAFETY: zeroed POD struct; proc_pidinfo writes at most `size` bytes.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if got == size {
        if info.pbi_start_tvsec != start_time || info.pbi_status == libc::SZOMB {
            return Identity::Gone;
        }
        return Identity::Same { uid: info.pbi_ruid };
    }
    // Fall back to a permission-only probe: signal 0 never touches the process.
    // SAFETY: kill with signal 0 only checks existence/permission.
    let alive = unsafe { libc::kill(pid, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    if alive {
        Identity::Unknown
    } else {
        Identity::Gone
    }
}

/// Send SIGTERM / SIGKILL to `p` after re-checking the policy and that the
/// pid still belongs to the same process, then wait briefly for it to exit.
#[cfg(target_os = "macos")]
pub fn quit(p: &ProcessInfo, me: &Me, mode: QuitMode) -> QuitOutcome {
    if let Control::Protected { reason, .. } = control(p, me) {
        return QuitOutcome::Refused(reason);
    }
    match identity(p.pid, p.start_time) {
        Identity::Gone => return QuitOutcome::AlreadyGone,
        Identity::Unknown => {
            return QuitOutcome::Refused("Nera couldn't confirm which process this is.")
        }
        Identity::Same { uid } if uid != me.uid => {
            return QuitOutcome::Refused("Owned by another user or a system account.")
        }
        Identity::Same { .. } => {}
    }
    // SAFETY: plain syscall; the target was verified just above.
    if unsafe { libc::kill(p.pid, mode.signal()) } != 0 {
        let err = std::io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::ESRCH) => QuitOutcome::AlreadyGone,
            Some(libc::EPERM) => QuitOutcome::Refused("macOS didn't allow Nera to quit it."),
            _ => QuitOutcome::Failed(err.to_string()),
        };
    }
    let deadline = Instant::now() + mode.wait();
    while Instant::now() < deadline {
        if identity(p.pid, p.start_time) == Identity::Gone {
            return QuitOutcome::Exited;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    QuitOutcome::StillRunning
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    fn me() -> Me {
        Me {
            uid: 501,
            pid: 4242,
            exe: Some(PathBuf::from(
                "/Applications/Nera Cleaner.app/Contents/MacOS/nera",
            )),
        }
    }

    fn proc(pid: i32, name: &str, uid: Option<u32>, exe: Option<&str>) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent: Some(1),
            name: name.into(),
            uid,
            user: uid.map(|u| u.to_string()).unwrap_or_default(),
            cpu: None,
            memory: None,
            exe: exe.map(PathBuf::from),
            bundle: None,
            start_time: 1_700_000_000,
            run_time: 10,
        }
    }

    fn app(pid: i32, name: &str, cpu: f32, memory: u64) -> ProcessInfo {
        let mut p = proc(
            pid,
            name,
            Some(501),
            Some(&format!("/Applications/{name}.app/Contents/MacOS/{name}")),
        );
        p.cpu = Some(cpu);
        p.memory = Some(memory);
        p.bundle = Some(BundleInfo {
            path: PathBuf::from(format!("/Applications/{name}.app")),
            name: name.into(),
            bundle_id: Some(format!("com.example.{}", name.to_lowercase())),
            background: false,
        });
        p
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn refuses_low_pids_root_other_users_and_self() {
        let me = me();
        assert!(!control(&proc(0, "kernel_task", Some(0), None), &me).can_quit());
        assert!(!control(&proc(1, "launchd", Some(0), Some("/sbin/launchd")), &me).can_quit());
        assert!(!control(&proc(300, "mdworker_shared", Some(0), None), &me).can_quit());
        assert!(!control(&proc(301, "coreaudiod", Some(202), None), &me).can_quit());
        assert!(!control(&proc(302, "thing", None, None), &me).can_quit());
        assert!(!control(&proc(4242, "nera", Some(501), None), &me).can_quit());
        // Another copy of Nera's own binary is protected too.
        assert!(!control(
            &proc(
                5000,
                "nera",
                Some(501),
                Some("/Applications/Nera Cleaner.app/Contents/MacOS/nera")
            ),
            &me
        )
        .can_quit());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn refuses_critical_and_system_components_owned_by_the_user() {
        let me = me();
        for name in ["WindowServer", "loginwindow", "cfprefsd", "distnoted"] {
            assert!(
                !control(&proc(900, name, Some(501), None), &me).can_quit(),
                "{name}"
            );
        }
        let agent = proc(901, "sharingd", Some(501), Some("/usr/libexec/sharingd"));
        assert!(!control(&agent, &me).can_quit());
        let xpc = proc(902, "com.apple.WebKit.WebContent", Some(501),
            Some("/System/Library/Frameworks/WebKit.framework/Versions/A/XPCServices/com.apple.WebKit.WebContent.xpc/Contents/MacOS/com.apple.WebKit.WebContent"));
        assert!(!control(&xpc, &me).can_quit());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn warns_for_finder_and_dock_and_allows_user_apps() {
        let me = me();
        let finder = proc(
            700,
            "Finder",
            Some(501),
            Some("/System/Library/CoreServices/Finder.app/Contents/MacOS/Finder"),
        );
        assert!(matches!(control(&finder, &me), Control::Caution(_)));
        let dock = proc(
            701,
            "Dock",
            Some(501),
            Some("/System/Library/CoreServices/Dock.app/Contents/MacOS/Dock"),
        );
        assert!(matches!(control(&dock, &me), Control::Caution(_)));
        assert_eq!(control(&app(702, "Safari", 1.0, 1), &me), Control::Allowed);
        let mail = proc(
            703,
            "Mail",
            Some(501),
            Some("/System/Applications/Mail.app/Contents/MacOS/Mail"),
        );
        assert_eq!(control(&mail, &me), Control::Allowed);
        let tool = proc(704, "python3", Some(501), Some("/usr/bin/python3"));
        assert_eq!(control(&tool, &me), Control::Allowed);
    }

    #[test]
    fn quit_never_signals_protected_processes() {
        let me = Me::current();
        // pid 1 (launchd) is refused before any syscall is made.
        let launchd = proc(1, "launchd", Some(0), Some("/sbin/launchd"));
        assert!(matches!(
            quit(&launchd, &me, QuitMode::Force),
            QuitOutcome::Refused(_)
        ));
        // Our own process is refused as well.
        let own = proc(me.pid, "nera", Some(me.uid), None);
        assert!(matches!(
            quit(&own, &me, QuitMode::Force),
            QuitOutcome::Refused(_)
        ));
    }

    #[test]
    fn quit_detects_pid_reuse_without_signalling() {
        let me = Me::current();
        // Our own test process, but with a start time that doesn't match: the
        // identity check reports it gone and no signal is sent. `me` uses a
        // fake pid so the self-protection rule doesn't short-circuit first.
        let fake_me = Me {
            pid: -1,
            exe: None,
            ..me.clone()
        };
        let mut target = proc(std::process::id() as i32, "cargo-test", Some(me.uid), None);
        target.start_time = 1;
        assert_eq!(
            quit(&target, &fake_me, QuitMode::Quit),
            QuitOutcome::AlreadyGone
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn identity_matches_the_running_test_process() {
        let mut sampler = Sampler::new();
        let snap = sampler.sample().expect("sample");
        let me = Me::current();
        let own = snap.find(me.pid).expect("own process listed");
        assert_eq!(own.uid, Some(me.uid));
        assert_eq!(
            identity(own.pid, own.start_time),
            Identity::Same { uid: me.uid }
        );
        assert_eq!(identity(own.pid, own.start_time + 1), Identity::Gone);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn samples_real_processes_read_only() {
        let mut sampler = Sampler::new();
        let first = sampler.sample().expect("sample");
        assert!(first.processes.len() > 10);
        assert!(
            first.memory_total > 0 && first.memory_used.is_some_and(|u| u <= first.memory_total)
        );
        assert!(first.cores >= 1);
        assert!((0.0..=100.0).contains(&first.cpu_load));
        let pids: std::collections::HashSet<_> = first.processes.iter().map(|p| p.pid).collect();
        assert_eq!(pids.len(), first.processes.len(), "pids are unique");
        // Our own process has memory and a CPU reading after the warm-up pair.
        let own = first.find(std::process::id() as i32).expect("own process");
        assert!(own.memory.is_some_and(|m| m > 0));
        assert!(own.cpu.is_some_and(|c| c >= 0.0));
        // Other users' processes are listed with a real owner but no counters.
        let launchd = first.find(1).expect("launchd");
        assert_eq!((launchd.uid, launchd.user.as_str()), (Some(0), "root"));
        assert!(launchd.cpu.is_none() && launchd.memory.is_none());
        // Owners resolve for (nearly) every process; a handful may exit mid-sample.
        let unknown = first
            .processes
            .iter()
            .filter(|p| p.user == "unknown")
            .count();
        assert!(
            unknown * 100 <= first.processes.len(),
            "{unknown} unresolved owners"
        );
        // The kernel is listed like in Activity Monitor, with no counters.
        let kernel = first.find(0).expect("kernel_task");
        assert_eq!(
            (kernel.name.as_str(), kernel.user.as_str()),
            ("kernel_task", "root")
        );
        assert!(kernel.cpu.is_none() && kernel.memory.is_none());
        // A second sample reuses previous counters.
        let second = sampler.sample().expect("second sample");
        assert!(!second.processes.is_empty());
    }

    #[test]
    fn filters_searches_and_sorts() {
        let mut daemon = proc(
            10,
            "cloudd",
            Some(501),
            Some("/System/Library/PrivateFrameworks/CloudDocsDaemon.framework/cloudd"),
        );
        daemon.cpu = Some(50.0);
        daemon.memory = Some(10);
        let mut root = proc(11, "syslogd", Some(0), Some("/usr/sbin/syslogd"));
        root.memory = Some(999);
        let list = vec![
            app(20, "Safari", 5.0, 500),
            app(21, "Notes", 1.0, 900),
            daemon,
            root,
        ];

        let apps = visible(&list, Filter::Apps, SortKey::Cpu, "", 501);
        assert_eq!(apps.iter().map(|p| p.pid).collect::<Vec<_>>(), vec![20, 21]);

        let mine = visible(&list, Filter::Mine, SortKey::Cpu, "", 501);
        assert_eq!(
            mine.iter().map(|p| p.pid).collect::<Vec<_>>(),
            vec![10, 20, 21]
        );

        let all_mem = visible(&list, Filter::All, SortKey::Memory, "", 501);
        assert_eq!(
            all_mem.iter().map(|p| p.pid).collect::<Vec<_>>(),
            vec![11, 21, 20, 10]
        );

        let by_name = visible(&list, Filter::All, SortKey::Name, "", 501);
        assert_eq!(
            by_name.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            vec!["cloudd", "Notes", "Safari", "syslogd"]
        );

        assert_eq!(
            visible(&list, Filter::All, SortKey::Cpu, "SAF", 501).len(),
            1
        );
        assert_eq!(
            visible(&list, Filter::All, SortKey::Cpu, "11", 501)[0].pid,
            11
        );
        assert_eq!(
            visible(&list, Filter::All, SortKey::Cpu, "com.example.notes", 501)[0].pid,
            21
        );
        assert!(visible(&list, Filter::Apps, SortKey::Cpu, "syslogd", 501).is_empty());
    }

    #[test]
    fn background_bundles_are_not_apps() {
        let mut agent = app(30, "Helper", 0.0, 0);
        agent.bundle.as_mut().unwrap().background = true;
        assert!(subtitle(&agent).ends_with("Background app"));
        assert!(!agent.is_app());
        assert!(app(31, "Safari", 0.0, 0).is_app());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn display_names_prefer_registered_app_then_full_process_name() {
        let app = BundleInfo {
            path: "/Applications/Zed.app".into(),
            name: "Zed".into(),
            bundle_id: None,
            background: false,
        };
        assert_eq!(
            display_name(
                "zed",
                Some(Path::new("/Applications/Zed.app/Contents/MacOS/zed")),
                Some(&app)
            ),
            "Zed"
        );
        // A registered background app keeps its process name.
        let agent = BundleInfo {
            background: true,
            ..app.clone()
        };
        assert_eq!(display_name("zed", None, Some(&agent)), "zed");
        // Kernel name is used as is (python3.14, not the symlink target's name).
        assert_eq!(
            display_name("python3.14", Some(Path::new("/x/bin/python")), None),
            "python3.14"
        );
        // Truncated kernel names are completed from the executable.
        assert_eq!(
            display_name(
                "com.apple.audio.Core-Audio-Drive",
                Some(Path::new(
                    "/x/com.apple.audio.Core-Audio-Driver-Service.helper"
                )),
                None
            ),
            "com.apple.audio.Core-Audio-Driver-Service.helper"
        );
        assert_eq!(
            display_name("zsh", Some(Path::new("/bin/zsh")), None),
            "zsh"
        );
    }

    #[test]
    fn formats_cpu_and_cells() {
        assert_eq!(format_cpu(12.345), "12.3%");
        assert_eq!(format_cpu(-1.0), "0.0%");
        assert_eq!(format_cpu_load(99.6), "100%");
        let mut p = proc(5, "x", Some(501), None);
        assert_eq!(
            (cpu_cell(&p).as_str(), memory_cell(&p).as_str()),
            ("\u{2014}", "\u{2014}")
        );
        p.cpu = Some(0.0);
        p.memory = Some(5 * 1024 * 1024);
        assert_eq!(
            (cpu_cell(&p).as_str(), memory_cell(&p).as_str()),
            ("0.0%", "5.0 MB")
        );
    }

    #[test]
    fn memory_uses_binary_units_and_never_prints_1024() {
        assert_eq!(format_memory(0), "0 bytes");
        assert_eq!(format_memory(1023), "1023 bytes");
        assert_eq!(format_memory(1024), "1 KB");
        assert_eq!(format_memory(1536), "2 KB");
        assert_eq!(format_memory(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_memory(68_719_476_736), "64.00 GB");
        assert_eq!(format_memory(1_122_225_728), "1.05 GB");
        // Rounds first, then promotes the unit.
        assert_eq!(format_memory(1024 * 1024 - 1), "1.0 MB");
        assert_eq!(format_memory(1024 * 1024 * 1024 - 1), "1.00 GB");
        assert_eq!(format_memory(1024 * 1024 * 1024 - 50_000), "1.00 GB");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn stats_text_reports_exact_figures() {
        let mut list = vec![app(1, "Safari", 5.0, 1), app(2, "Notes", 1.0, 1)];
        list.push(proc(3, "cloudd", Some(501), None));
        list.push(proc(4, "syslogd", Some(0), None));
        let snap = Snapshot {
            processes: list,
            cpu_load: 41.6,
            cores: 10,
            memory_used: Some(32 * 1024 * 1024 * 1024),
            memory_total: 64 * 1024 * 1024 * 1024,
            apps_known: true,
        };
        let t = stats_text(&snap, 501);
        assert_eq!(
            (t.cpu_value.as_str(), t.cpu_caption.as_str()),
            ("42%", "Across 10 cores")
        );
        assert_eq!(
            (t.memory_value.as_str(), t.memory_caption.as_str()),
            ("32.00 GB", "of 64.00 GB \u{b7} 50% used")
        );
        assert_eq!(
            (t.processes_value.as_str(), t.processes_caption.as_str()),
            ("4", "2 apps \u{b7} 3 yours")
        );
        let unknown = stats_text(
            &Snapshot {
                apps_known: false,
                cores: 1,
                ..snap.clone()
            },
            501,
        );
        assert_eq!(unknown.processes_caption, "3 yours");
        assert_eq!(unknown.cpu_caption, "Across 1 core");
        // Unreadable page counters are shown as unknown, never as another definition.
        let no_mem = stats_text(
            &Snapshot {
                memory_used: None,
                ..snap
            },
            501,
        );
        assert_eq!(
            (no_mem.memory_value.as_str(), no_mem.memory_caption.as_str()),
            ("\u{2014}", "of 64.00 GB")
        );
    }

    #[test]
    fn unknown_figures_sort_last() {
        let mut known = proc(1, "a", Some(501), None);
        known.cpu = Some(0.0);
        let unknown = proc(2, "b", Some(501), None);
        let list = vec![unknown.clone(), known.clone()];
        let sorted = visible(&list, Filter::All, SortKey::Cpu, "", 501);
        assert_eq!(sorted.iter().map(|p| p.pid).collect::<Vec<_>>(), vec![1, 2]);
    }
}
