//! Windows half of the processes engine: sampling, the quit policy and
//! quitting. Shared types, filtering and formatting live in `engine.rs`.
//!
//! Every figure follows Task Manager's Details tab:
//! * CPU is the change in a process's kernel + user time between two
//!   snapshots, divided by the elapsed time × the logical processors, so all
//!   processes together add up to 100 %. A process first seen in this
//!   sample has no CPU figure.
//! * Memory is the private working set (Task Manager's "Memory (private
//!   working set)"), in binary units.
//! * Both come from one `SystemProcessInformation` snapshot that covers every
//!   process, protected and other accounts' ones included.
//! * System memory in use is Task Manager's "In use".
//! * Apps are processes with a window on the taskbar.
//!
//! Windows only tells elevated apps who runs system and service processes;
//! those rows have no user and are never offered for quitting.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER};
use windows_sys::Win32::System::Threading::{
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};

use super::{
    BundleInfo, Control, Me, ProcessInfo, QuitMode, QuitOutcome, Snapshot, MIN_WINDOW, STALE_AFTER,
};
use crate::sys;

impl Me {
    pub fn current() -> Self {
        Self {
            uid: sys::current_sid().map_or(u32::MAX, |s| sys::sid_id(&s)),
            pid: std::process::id() as i32,
            exe: std::env::current_exe()
                .ok()
                .map(nera_core::paths::standardize),
        }
    }
}

// MARK: - Sampling

/// CPU counter of the previous snapshot, per pid.
struct Prev {
    create_time: u64,
    cpu_time: u64,
}

/// What opening a process revealed; kept per (pid, creation time) so each
/// process is opened once, not on every sample.
#[derive(Clone, Default)]
struct Opened {
    exe: Option<PathBuf>,
    sid: Option<String>,
}

/// Long-lived sampler. Keep one per screen and call [`Sampler::sample`] from
/// a background thread; the previous counters it keeps make CPU deltas right.
pub struct Sampler {
    prev: HashMap<u32, Prev>,
    prev_at: Option<Instant>,
    prev_system: Option<(u64, u64)>,
    opened: HashMap<(u32, u64), Opened>,
    user_names: HashMap<String, String>,
    descriptions: HashMap<PathBuf, Option<String>>,
    processors: usize,
    last: Option<Instant>,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

/// One snapshot with the CPU figures derived from the previous one.
struct Measured {
    entries: Vec<sys::ProcEntry>,
    cpu: HashMap<u32, f32>,
    load: Option<f32>,
}

impl Sampler {
    pub fn new() -> Self {
        Self {
            prev: HashMap::new(),
            prev_at: None,
            prev_system: None,
            opened: HashMap::new(),
            user_names: HashMap::new(),
            descriptions: HashMap::new(),
            processors: sys::logical_processors(),
            last: None,
        }
    }

    /// Refresh everything and return a snapshot. Blocks briefly on the first
    /// call (and after a long pause) to measure CPU over a short window.
    pub fn sample(&mut self) -> anyhow::Result<Snapshot> {
        let stale = self.last.is_none_or(|t| t.elapsed() > STALE_AFTER);
        if stale {
            self.measure()?;
            std::thread::sleep(MIN_WINDOW);
        } else if let Some(wait) = self.last.and_then(|t| MIN_WINDOW.checked_sub(t.elapsed())) {
            std::thread::sleep(wait);
        }
        let m = self.measure()?;
        self.last = Some(Instant::now());
        let apps = sys::app_pids();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());

        let live: HashSet<(u32, u64)> = m.entries.iter().map(|e| (e.pid, e.create_time)).collect();
        self.opened.retain(|k, _| live.contains(k));

        let mut processes = Vec::with_capacity(m.entries.len());
        for e in &m.entries {
            // The System Idle Process is idle time, not a process.
            if e.pid == 0 {
                continue;
            }
            let opened = self
                .opened
                .entry((e.pid, e.create_time))
                .or_insert_with(|| inspect(e.pid, e.create_time))
                .clone();
            let exe_name = opened
                .exe
                .as_deref()
                .and_then(Path::file_name)
                .map(|f| f.to_string_lossy().into_owned());
            let image = if e.name.is_empty() {
                exe_name.clone().unwrap_or_default()
            } else {
                e.name.clone()
            };
            let bundle = apps
                .as_ref()
                .is_some_and(|a| a.contains(&e.pid))
                .then(|| BundleInfo {
                    path: opened.exe.clone().unwrap_or_default(),
                    name: opened
                        .exe
                        .as_deref()
                        .and_then(|exe| self.description(exe))
                        .or_else(|| exe_name.clone())
                        .unwrap_or_else(|| image.clone()),
                    bundle_id: None,
                    background: false,
                });
            let name = bundle.as_ref().map_or(image, |b| b.name.clone());
            if name.is_empty() {
                continue;
            }
            let uid = opened.sid.as_deref().map(sys::sid_id);
            let user = opened
                .sid
                .as_deref()
                .map(|sid| self.user_name(sid))
                .unwrap_or_default();
            let start_time = sys::filetime_to_unix(e.create_time);
            processes.push(ProcessInfo {
                pid: e.pid as i32,
                parent: (e.parent != 0).then_some(e.parent as i32),
                name,
                uid,
                user,
                cpu: m
                    .cpu
                    .get(&e.pid)
                    .copied()
                    .filter(|c| c.is_finite())
                    .map(|c| c.max(0.0)),
                memory: Some(e.private_working_set),
                exe: opened.exe,
                bundle,
                start_time,
                run_time: now.saturating_sub(start_time),
            });
        }
        if processes.is_empty() {
            anyhow::bail!("Windows returned an empty process list");
        }

        let memory = sys::memory_status();
        let memory_used = memory.and_then(|(total, available)| {
            sys::modified_bytes().map(|modified| sys::in_use(total, available, modified))
        });
        Ok(Snapshot {
            processes,
            cpu_load: m.load.unwrap_or(0.0).clamp(0.0, 100.0),
            cores: self.processors,
            memory_used,
            memory_total: memory.map_or(0, |(total, _)| total),
            apps_known: apps.is_some(),
        })
    }

    /// Take a snapshot and turn the change since the previous one into CPU %.
    fn measure(&mut self) -> anyhow::Result<Measured> {
        let entries = sys::processes().map_err(|status| {
            anyhow::anyhow!("Windows didn't return the process list (status {status:#010x})")
        })?;
        let at = Instant::now();
        let system = sys::system_times();
        let wall = self
            .prev_at
            .map(|prev| at.duration_since(prev).as_nanos() as u64);
        let mut cpu = HashMap::with_capacity(entries.len());
        let mut next = HashMap::with_capacity(entries.len());
        for e in &entries {
            if let (Some(prev), Some(wall)) = (self.prev.get(&e.pid), wall) {
                if prev.create_time == e.create_time && e.cpu_time >= prev.cpu_time {
                    cpu.insert(
                        e.pid,
                        sys::cpu_percent(e.cpu_time - prev.cpu_time, wall, self.processors),
                    );
                }
            }
            next.insert(
                e.pid,
                Prev {
                    create_time: e.create_time,
                    cpu_time: e.cpu_time,
                },
            );
        }
        let load = match (self.prev_system, system) {
            (Some(before), Some(after)) => sys::load_percent(before, after),
            _ => None,
        };
        self.prev = next;
        self.prev_at = Some(at);
        self.prev_system = system;
        Ok(Measured { entries, cpu, load })
    }

    /// Account name for a SID, cached (domain lookups can be slow).
    fn user_name(&mut self, sid: &str) -> String {
        self.user_names
            .entry(sid.to_string())
            .or_insert_with(|| sys::account_name(sid).unwrap_or_default())
            .clone()
    }

    /// File description of an executable, cached per path.
    fn description(&mut self, exe: &Path) -> Option<String> {
        self.descriptions
            .entry(exe.to_path_buf())
            .or_insert_with(|| sys::file_description(exe))
            .clone()
    }
}

/// Executable and owner of a process, if Windows lets us open it. Nothing is
/// returned when the pid now belongs to a newer process.
fn inspect(pid: u32, create_time: u64) -> Opened {
    let Ok(process) = sys::Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION) else {
        return Opened::default();
    };
    if process.creation_time() != Some(create_time) {
        return Opened::default();
    }
    Opened {
        exe: process.exe(),
        sid: process.owner().map(|o| o.sid),
    }
}

// MARK: - Search and row text

/// File name of the process's executable, or its image name when Windows
/// doesn't share the path (Task Manager's Details "Name").
fn image_name(p: &ProcessInfo) -> String {
    p.exe
        .as_deref()
        .and_then(Path::file_name)
        .map_or_else(|| p.name.clone(), |f| f.to_string_lossy().into_owned())
}

/// True when `query` (case-insensitive) matches the name, executable,
/// owner or pid.
pub fn matches_query(p: &ProcessInfo, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    p.name.to_lowercase().contains(&q)
        || image_name(p).to_lowercase().contains(&q)
        || p.user.to_lowercase() == q
        || p.pid.to_string() == q
        || p.bundle
            .as_ref()
            .and_then(|b| b.bundle_id.as_deref())
            .is_some_and(|id| id.to_lowercase().contains(&q))
}

/// The row's second line: "PID 123 · chrome.exe · user". The executable is
/// left out when it is already the name, the user when Windows hides it.
pub fn subtitle(p: &ProcessInfo) -> String {
    let mut parts = vec![format!("PID {}", p.pid)];
    let image = image_name(p);
    if image != p.name {
        parts.push(image);
    }
    if !p.user.is_empty() {
        parts.push(p.user.clone());
    }
    if p.bundle.as_ref().is_some_and(|b| b.background) {
        parts.push("Background app".into());
    }
    parts.join(" \u{b7} ")
}

// MARK: - Quit policy

/// Processes Windows can't run without (or that would end the session),
/// matched case-insensitively against the image name.
const CRITICAL: &[&str] = &[
    "System",
    "Secure System",
    "Registry",
    "Memory Compression",
    "smss.exe",
    "csrss.exe",
    "wininit.exe",
    "winlogon.exe",
    "services.exe",
    "lsass.exe",
    "LsaIso.exe",
    "svchost.exe",
    "dwm.exe",
    "fontdrvhost.exe",
    "sihost.exe",
    "LogonUI.exe",
    "userinit.exe",
    "ctfmon.exe",
    "audiodg.exe",
    "MsMpEng.exe",
];

/// Runs the taskbar and desktop as well as File Explorer windows.
const SHELL: &[&str] = &["explorer.exe"];

/// Tools that live in the Windows folder but that users start and may stop.
const USER_TOOLS: &[&str] = &[
    "cmd.exe",
    "powershell.exe",
    "powershell_ise.exe",
    "notepad.exe",
    "mspaint.exe",
    "wsl.exe",
    "mmc.exe",
    "regedit.exe",
    "mstsc.exe",
    "charmap.exe",
    "cscript.exe",
    "wscript.exe",
    "write.exe",
];

/// The Windows folder (`%SystemRoot%`).
fn system_root() -> PathBuf {
    std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("windir"))
        .map(PathBuf::from)
        .unwrap_or_else(|| nera_core::paths::system_drive().join("Windows"))
}

fn same_path(a: &Path, b: &Path) -> bool {
    nera_core::paths::is_under(a, b) && nera_core::paths::is_under(b, a)
}

/// The quit policy: only the current user's own, non-critical processes;
/// never System, protected or other accounts' processes, and never Nera.
pub fn control(p: &ProcessInfo, me: &Me) -> Control {
    const SYSTEM: Control = Control::Protected {
        badge: "System",
        reason: "Windows needs this process to keep running.",
    };
    // 0 is the System Idle Process, 4 the kernel ("System").
    if p.pid <= 4 {
        return SYSTEM;
    }
    let is_me =
        p.pid == me.pid || matches!((&p.exe, &me.exe), (Some(a), Some(b)) if same_path(a, b));
    if is_me {
        return Control::Protected {
            badge: "Nera",
            reason: "This is Nera Cleaner.",
        };
    }
    let image = image_name(p);
    let named = |list: &[&str]| list.iter().any(|n| n.eq_ignore_ascii_case(&image));
    if named(CRITICAL) {
        return SYSTEM;
    }
    match p.uid {
        None => {
            return Control::Protected {
                badge: "Restricted",
                reason: "Windows doesn't let Nera see who runs this process.",
            }
        }
        Some(0) => {
            return Control::Protected {
                badge: "System",
                reason: "Runs under the Windows system account.",
            }
        }
        Some(uid) if uid != me.uid => {
            return Control::Protected {
                badge: "Other user",
                reason: "Owned by another user or a system account.",
            }
        }
        _ => {}
    }
    if named(SHELL) {
        return Control::Caution("File Explorer also runs your taskbar and desktop.");
    }
    let in_windows = p
        .exe
        .as_deref()
        .is_some_and(|e| nera_core::paths::is_under(e, system_root()));
    if in_windows && !p.is_app() && !named(USER_TOOLS) {
        return Control::Protected {
            badge: "System",
            reason: "This is part of Windows.",
        };
    }
    Control::Allowed
}

// MARK: - Quitting

fn os_error(code: u32) -> String {
    std::io::Error::from_raw_os_error(code as i32).to_string()
}

/// Ask `p` to close (WM_CLOSE to its windows) or end it (TerminateProcess)
/// after re-checking the policy, that the pid still belongs to the same
/// process and that it is the user's own; then wait briefly for it to exit.
pub fn quit(p: &ProcessInfo, me: &Me, mode: QuitMode) -> QuitOutcome {
    if let Control::Protected { reason, .. } = control(p, me) {
        return QuitOutcome::Refused(reason);
    }
    let pid = p.pid as u32;
    // Holding this handle keeps the pid from being reused until we're done.
    let process =
        match sys::Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE) {
            Ok(process) => process,
            Err(ERROR_INVALID_PARAMETER) => return QuitOutcome::AlreadyGone,
            Err(ERROR_ACCESS_DENIED) => {
                return QuitOutcome::Refused("Windows didn't let Nera open this process.")
            }
            Err(code) => return QuitOutcome::Failed(os_error(code)),
        };
    match process.creation_time() {
        Some(created) if sys::filetime_to_unix(created) == p.start_time => {}
        Some(_) => return QuitOutcome::AlreadyGone,
        None => return QuitOutcome::Refused("Nera couldn't confirm which process this is."),
    }
    if process.wait(0) {
        return QuitOutcome::AlreadyGone;
    }
    match process.owner() {
        Some(owner) if sys::sid_id(&owner.sid) == me.uid => {}
        Some(_) => return QuitOutcome::Refused("Owned by another user or a system account."),
        None => return QuitOutcome::Refused("Nera couldn't confirm which process this is."),
    }
    match mode {
        QuitMode::Quit => {
            if sys::close_windows(pid) == 0 {
                return QuitOutcome::Refused(
                    "It has no open window to close. Use Force quit to stop it.",
                );
            }
        }
        QuitMode::Force => {
            // The handle above pins the pid, so this opens the same process.
            let killer = match sys::Process::open(pid, PROCESS_TERMINATE) {
                Ok(killer) => killer,
                Err(ERROR_ACCESS_DENIED) => {
                    return QuitOutcome::Refused(
                        "Windows didn't let Nera end it. It may be running as administrator.",
                    )
                }
                Err(code) => return QuitOutcome::Failed(os_error(code)),
            };
            if let Err(code) = killer.terminate() {
                return if process.wait(0) {
                    QuitOutcome::AlreadyGone
                } else if code == ERROR_ACCESS_DENIED {
                    QuitOutcome::Refused(
                        "Windows didn't let Nera end it. It may be running as administrator.",
                    )
                } else {
                    QuitOutcome::Failed(os_error(code))
                };
            }
        }
    }
    let wait_ms = u32::try_from(mode.wait().as_millis()).unwrap_or(u32::MAX);
    if process.wait(wait_ms) {
        QuitOutcome::Exited
    } else {
        QuitOutcome::StillRunning
    }
}

#[cfg(test)]
mod tests {
    use super::super::{stats_text, visible, Filter, SortKey, CORE_NOUN, MEMORY_TILE};
    use super::*;

    const ME: u32 = 1000;

    fn me() -> Me {
        Me {
            uid: ME,
            pid: 4242,
            exe: Some(PathBuf::from(r"C:\Program Files\Nera Cleaner\nera.exe")),
        }
    }

    fn proc(pid: i32, name: &str, uid: Option<u32>, exe: Option<&str>) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent: Some(1),
            name: name.into(),
            uid,
            user: if uid == Some(ME) {
                "Admin".into()
            } else {
                String::new()
            },
            cpu: None,
            memory: Some(1024),
            exe: exe.map(PathBuf::from),
            bundle: None,
            start_time: 1_700_000_000,
            run_time: 10,
        }
    }

    fn app(pid: i32, name: &str, exe: &str) -> ProcessInfo {
        let mut p = proc(pid, name, Some(ME), Some(exe));
        p.bundle = Some(BundleInfo {
            path: PathBuf::from(exe),
            name: name.into(),
            bundle_id: None,
            background: false,
        });
        p
    }

    #[test]
    fn protects_the_kernel_critical_processes_and_self() {
        let me = me();
        for (pid, name) in [
            (0, ""),
            (4, "System"),
            (188, "Secure System"),
            (228, "Registry"),
        ] {
            assert!(
                !control(&proc(pid, name, None, None), &me).can_quit(),
                "{name}"
            );
        }
        for name in [
            "smss.exe",
            "csrss.exe",
            "wininit.exe",
            "winlogon.exe",
            "services.exe",
            "LSASS.EXE",
        ] {
            assert_eq!(
                control(&proc(900, name, None, None), &me),
                Control::Protected {
                    badge: "System",
                    reason: "Windows needs this process to keep running."
                },
                "{name}"
            );
        }
        // A user-owned svchost / sihost is still critical.
        let sihost = proc(
            901,
            "sihost.exe",
            Some(ME),
            Some(r"C:\Windows\System32\sihost.exe"),
        );
        assert!(!control(&sihost, &me).can_quit());
        // Nera itself, by pid or by executable (any case).
        assert!(!control(&proc(4242, "nera.exe", Some(ME), None), &me).can_quit());
        let copy = proc(
            5000,
            "nera.exe",
            Some(ME),
            Some(r"c:\program files\nera cleaner\NERA.EXE"),
        );
        assert!(matches!(
            control(&copy, &me),
            Control::Protected { badge: "Nera", .. }
        ));
    }

    #[test]
    fn protects_unknown_system_and_other_owners() {
        let me = me();
        let hidden = proc(1000, "IntelCpHDCPSvc.exe", None, None);
        assert!(matches!(
            control(&hidden, &me),
            Control::Protected {
                badge: "Restricted",
                ..
            }
        ));
        let service = proc(
            1001,
            "vendor-service.exe",
            Some(0),
            Some(r"C:\Vendor\svc.exe"),
        );
        assert!(matches!(
            control(&service, &me),
            Control::Protected {
                badge: "System",
                ..
            }
        ));
        let other = proc(1002, "app.exe", Some(1001), Some(r"C:\Apps\app.exe"));
        assert!(matches!(
            control(&other, &me),
            Control::Protected {
                badge: "Other user",
                ..
            }
        ));
    }

    #[test]
    fn windows_components_are_protected_unless_apps_or_user_tools() {
        let me = me();
        let root = system_root();
        let sys32 = |f: &str| root.join("System32").join(f).to_string_lossy().into_owned();
        let broker = proc(
            10,
            "RuntimeBroker.exe",
            Some(ME),
            Some(&sys32("RuntimeBroker.exe")),
        );
        assert!(!control(&broker, &me).can_quit());
        let cmd = proc(11, "cmd.exe", Some(ME), Some(&sys32("cmd.exe")));
        assert_eq!(control(&cmd, &me), Control::Allowed);
        let shell = proc(
            12,
            "PowerShell.exe",
            Some(ME),
            Some(&sys32(r"WindowsPowerShell\v1.0\powershell.exe")),
        );
        assert_eq!(control(&shell, &me), Control::Allowed);
        let settings = app(
            13,
            "Settings",
            &root
                .join(r"ImmersiveControlPanel\SystemSettings.exe")
                .to_string_lossy(),
        );
        assert_eq!(control(&settings, &me), Control::Allowed);
        let explorer = app(
            14,
            "Windows Explorer",
            &root.join("explorer.exe").to_string_lossy(),
        );
        assert!(matches!(control(&explorer, &me), Control::Caution(_)));
        let chrome = app(
            15,
            "Google Chrome",
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        );
        assert_eq!(control(&chrome, &me), Control::Allowed);
        let tool = proc(
            16,
            "node.exe",
            Some(ME),
            Some(r"C:\Program Files\nodejs\node.exe"),
        );
        assert_eq!(control(&tool, &me), Control::Allowed);
    }

    #[test]
    fn quit_refuses_protected_processes_without_opening_them() {
        let me = Me::current();
        let system = proc(4, "System", None, None);
        assert!(matches!(
            quit(&system, &me, QuitMode::Force),
            QuitOutcome::Refused(_)
        ));
        let own = proc(me.pid, "nera.exe", Some(me.uid), None);
        assert!(matches!(
            quit(&own, &me, QuitMode::Force),
            QuitOutcome::Refused(_)
        ));
    }

    #[test]
    fn quit_detects_pid_reuse_and_vanished_processes() {
        let me = Me::current();
        let fake_me = Me {
            pid: -1,
            exe: None,
            ..me.clone()
        };
        // Our own test process with a start time that doesn't match: nothing is sent.
        let mut target = proc(std::process::id() as i32, "cargo-test", Some(me.uid), None);
        target.start_time = 1;
        assert_eq!(
            quit(&target, &fake_me, QuitMode::Force),
            QuitOutcome::AlreadyGone
        );
        // A pid far beyond any the system hands out.
        let ghost = proc(2_000_000_000, "ghost.exe", Some(me.uid), None);
        assert_eq!(
            quit(&ghost, &fake_me, QuitMode::Quit),
            QuitOutcome::AlreadyGone
        );
    }

    #[test]
    fn samples_real_processes_read_only() {
        let mut sampler = Sampler::new();
        let first = sampler.sample().expect("sample");
        assert!(first.processes.len() > 10);
        assert!(first.memory_total > 0);
        assert!(first
            .memory_used
            .is_some_and(|u| u > 0 && u <= first.memory_total));
        assert!(first.cores >= 1);
        assert!((0.0..=100.0).contains(&first.cpu_load));
        let pids: HashSet<_> = first.processes.iter().map(|p| p.pid).collect();
        assert_eq!(pids.len(), first.processes.len(), "pids are unique");
        assert!(first.find(0).is_none(), "the idle process is not listed");
        // Every process has a memory figure; ours a CPU reading after the warm-up pair.
        assert!(first.processes.iter().all(|p| p.memory.is_some()));
        let me = Me::current();
        let own = first.find(me.pid).expect("own process");
        assert_eq!(own.uid, Some(me.uid));
        assert!(!own.user.is_empty());
        assert!(own.memory.is_some_and(|m| m > 0));
        assert!(own.cpu.is_some_and(|c| (0.0..=100.0).contains(&c)));
        assert!(own.exe.is_some());
        assert!(own.start_time > 1_600_000_000);
        // The kernel is listed under its Task Manager name and is protected.
        let system = first.find(4).expect("System");
        assert_eq!(system.name, "System");
        assert!(!control(system, &me).can_quit());
        // Per-process CPU never adds up to more than the whole machine.
        let total: f32 = first.processes.iter().filter_map(|p| p.cpu).sum();
        assert!(total <= 101.0, "{total}");
        let second = sampler.sample().expect("second sample");
        assert!(!second.processes.is_empty());
        assert!(second.find(me.pid).is_some_and(|p| p.cpu.is_some()));
    }

    #[test]
    fn subtitles_and_search_use_the_executable_name() {
        let chrome = app(20, "Google Chrome", r"C:\Program Files\Google\chrome.exe");
        assert_eq!(subtitle(&chrome), "PID 20 \u{b7} chrome.exe \u{b7} Admin");
        let svc = proc(21, "svchost.exe", None, None);
        assert_eq!(subtitle(&svc), "PID 21");
        let list = vec![chrome, svc];
        let hits = visible(&list, Filter::All, SortKey::Name, "CHROME.EXE", ME);
        assert_eq!(hits.iter().map(|p| p.pid).collect::<Vec<_>>(), vec![20]);
        assert_eq!(
            visible(&list, Filter::All, SortKey::Name, "admin", ME).len(),
            1
        );
        assert_eq!(visible(&list, Filter::Mine, SortKey::Name, "", ME).len(), 1);
        assert_eq!(visible(&list, Filter::Apps, SortKey::Name, "", ME).len(), 1);
    }

    #[test]
    fn stats_text_uses_task_manager_words() {
        let snap = Snapshot {
            processes: vec![
                app(1, "Google Chrome", r"C:\c\chrome.exe"),
                proc(2, "node.exe", Some(ME), None),
                proc(3, "svchost.exe", None, None),
            ],
            cpu_load: 12.4,
            cores: 12,
            memory_used: Some(8 * 1024 * 1024 * 1024),
            memory_total: 32 * 1024 * 1024 * 1024,
            apps_known: true,
        };
        let t = stats_text(&snap, ME);
        assert_eq!(
            (t.cpu_value.as_str(), t.cpu_caption.as_str()),
            ("12%", "Across 12 logical processors")
        );
        assert_eq!(
            (t.memory_value.as_str(), t.memory_caption.as_str()),
            ("8.00 GB", "of 32.00 GB \u{b7} 25% used")
        );
        assert_eq!(
            (t.processes_value.as_str(), t.processes_caption.as_str()),
            ("3", "1 apps \u{b7} 2 yours")
        );
        assert_eq!(
            (CORE_NOUN, MEMORY_TILE),
            ("logical processor", "Memory in use")
        );
    }
}
