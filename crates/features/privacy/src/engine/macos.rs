//! macOS side of the privacy engine: where browsers keep their data under
//! `~/Library`, Safari (built in, behind Full Disk Access), the Apple menu's
//! recent-items lists and the running-app rule for `.app` bundles.
//!
//! Moved here unchanged from `engine.rs`; only the browser table now resolves
//! its folders to absolute paths so the shared scanners take them as given.

use super::{item, Access, Chromium, DataKind, Engine, Firefox, Group, Spec};
use nera_core::fs_util::{self, Cancel};
use nera_core::paths;
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

/// Pane of System Settings where Full Disk Access is granted.
pub const FULL_DISK_ACCESS_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";

/// Where to look. [`Env::real`] for the app, fixture dirs in tests.
#[derive(Clone, Debug)]
pub struct Env {
    pub home: PathBuf,
    /// Folders searched for `<Name>.app`.
    pub app_dirs: Vec<PathBuf>,
}

impl Env {
    pub fn real() -> Self {
        let home = paths::home();
        Self {
            app_dirs: vec![
                PathBuf::from("/Applications"),
                home.join("Applications"),
                PathBuf::from("/System/Volumes/Preboot/Cryptexes/App/System/Applications"),
                PathBuf::from("/System/Applications"),
            ],
            home,
        }
    }

    fn lib(&self, rel: &str) -> PathBuf {
        self.home.join("Library").join(rel)
    }

    fn app_support(&self, rel: &str) -> PathBuf {
        self.home.join("Library/Application Support").join(rel)
    }

    fn caches(&self, rel: &str) -> PathBuf {
        self.home.join("Library/Caches").join(rel)
    }

    pub(super) fn find_app(&self, app: &str) -> Option<PathBuf> {
        self.app_dirs
            .iter()
            .map(|d| d.join(app))
            .find(|p| p.is_dir())
    }
}

// MARK: - Browser catalogue

pub(super) fn specs(env: &Env) -> Vec<Spec> {
    vec![
        Spec {
            id: "safari",
            name: "Safari",
            app: "Safari.app",
            icon: "compass",
            engine: Engine::Safari,
        },
        Spec {
            id: "chrome",
            name: "Google Chrome",
            app: "Google Chrome.app",
            icon: "globe",
            engine: Engine::Chromium(Chromium {
                support: env.app_support("Google/Chrome"),
                cache_root: Some(env.caches("Google/Chrome")),
                flat_caches: vec![],
                cache_mirror: None,
                root_is_profile: false,
            }),
        },
        Spec {
            id: "firefox",
            name: "Firefox",
            app: "Firefox.app",
            icon: "flame",
            engine: Engine::Firefox(Firefox {
                support: env.app_support("Firefox"),
                cache_root: env.caches("Firefox"),
            }),
        },
        Spec {
            id: "edge",
            name: "Microsoft Edge",
            app: "Microsoft Edge.app",
            icon: "globe",
            engine: Engine::Chromium(Chromium {
                support: env.app_support("Microsoft Edge"),
                cache_root: Some(env.caches("Microsoft Edge")),
                flat_caches: vec![],
                cache_mirror: None,
                root_is_profile: false,
            }),
        },
        Spec {
            id: "brave",
            name: "Brave",
            app: "Brave Browser.app",
            icon: "shield",
            engine: Engine::Chromium(Chromium {
                support: env.app_support("BraveSoftware/Brave-Browser"),
                cache_root: Some(env.caches("BraveSoftware/Brave-Browser")),
                flat_caches: vec![],
                cache_mirror: None,
                root_is_profile: false,
            }),
        },
        Spec {
            id: "arc",
            name: "Arc",
            app: "Arc.app",
            icon: "globe",
            engine: Engine::Chromium(Chromium {
                support: env.app_support("Arc/User Data"),
                cache_root: Some(env.caches("Arc/User Data")),
                flat_caches: vec![env.caches("company.thebrowser.Browser")],
                cache_mirror: None,
                root_is_profile: false,
            }),
        },
        Spec {
            id: "opera",
            name: "Opera",
            app: "Opera.app",
            icon: "circle-dot",
            engine: Engine::Chromium(Chromium {
                support: env.app_support("com.operasoftware.Opera"),
                cache_root: None,
                flat_caches: vec![env.caches("com.operasoftware.Opera")],
                cache_mirror: None,
                root_is_profile: true,
            }),
        },
        Spec {
            id: "vivaldi",
            name: "Vivaldi",
            app: "Vivaldi.app",
            icon: "globe",
            engine: Engine::Chromium(Chromium {
                support: env.app_support("Vivaldi"),
                cache_root: Some(env.caches("Vivaldi")),
                flat_caches: vec![],
                cache_mirror: None,
                root_is_profile: false,
            }),
        },
    ]
}

/// Full Disk Access as seen from this process.
pub(super) fn full_disk_access(env: &Env) -> Access {
    probe_full_disk_access(&env.home)
}

/// Groups that are not browsers: the Apple menu's recent-items lists.
pub(super) fn system_groups(env: &Env, cancel: &Cancel, progress: &dyn Fn(&str)) -> Vec<Group> {
    progress("Measuring recent items");
    vec![scan_recent(env, cancel)]
}

/// Allocated size of `path` counting every hard-linked file once, like `du`.
/// (`seen` holds the `(device, inode)` of multiply-linked files already counted;
/// WebKit's cache stores each blob twice as hard links.) Symlinks are not followed.
pub(super) fn unique_size(path: &Path, seen: &mut HashSet<(u64, u64)>, cancel: &Cancel) -> u64 {
    fn once(meta: &std::fs::Metadata, seen: &mut HashSet<(u64, u64)>) -> u64 {
        if meta.is_file() && meta.nlink() > 1 && !seen.insert((meta.dev(), meta.ino())) {
            return 0;
        }
        fs_util::allocated_size(meta)
    }
    let mut total = 0u64;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    total += once(&meta, seen);
    if !meta.is_dir() {
        return total;
    }
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
            total += once(&meta, seen);
            if meta.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    total
}

pub(super) fn scan_safari(
    env: &Env,
    spec: &Spec,
    has_app: bool,
    cancel: &Cancel,
    progress: &dyn Fn(&str),
) -> Group {
    let _ = has_app; // Safari ships with macOS; always listed.
    progress("Measuring Safari");
    let safari = env.lib("Safari");
    let container = env.lib("Containers/com.apple.Safari/Data/Library");
    let with_sidecars = |base: PathBuf| -> Vec<PathBuf> {
        ["", "-wal", "-shm", "-journal", "-lock"]
            .iter()
            .map(|suffix| {
                let mut name = base.file_name().unwrap_or_default().to_os_string();
                name.push(suffix);
                base.with_file_name(name)
            })
            .collect()
    };
    let items = vec![
        item(
            spec.id,
            DataKind::Cache,
            vec![env.caches("com.apple.Safari"), container.join("Caches")],
            true,
            cancel,
        ),
        item(
            spec.id,
            DataKind::Cookies,
            vec![
                env.lib("Cookies/Cookies.binarycookies"),
                container.join("Cookies/Cookies.binarycookies"),
            ],
            true,
            cancel,
        ),
        item(
            spec.id,
            DataKind::History,
            {
                let mut v = with_sidecars(safari.join("History.db"));
                v.push(safari.join("RecentlyClosedTabs.plist"));
                v
            },
            true,
            cancel,
        ),
        item(
            spec.id,
            DataKind::WebsiteData,
            vec![
                safari.join("LocalStorage"),
                safari.join("Databases"),
                env.lib("WebKit/com.apple.Safari/WebsiteData"),
                container.join("WebKit/WebsiteData"),
            ],
            true,
            cancel,
        ),
        item(
            spec.id,
            DataKind::DownloadsHistory,
            vec![safari.join("Downloads.plist")],
            true,
            cancel,
        ),
        item(
            spec.id,
            DataKind::FormData,
            vec![safari.join("Form Values")],
            true,
            cancel,
        ),
    ];
    Group {
        id: spec.id.into(),
        name: spec.name.into(),
        icon: spec.icon,
        detail: "Built into macOS".into(),
        app_name: Some(spec.app),
        installed: true,
        running: false,
        items,
    }
}

// MARK: - Recent items

/// Classify a file in `com.apple.sharedfilelist`. Only the "recent" lists
/// qualify: Finder favourites, iCloud and project lists share the folder and
/// must never be touched.
pub fn classify_recent(file_name: &str) -> Option<DataKind> {
    let rest = file_name.strip_prefix("com.apple.LSSharedFileList.")?;
    let stem = rest.split(".sfl").next().unwrap_or(rest);
    if !rest.contains(".sfl") {
        return None;
    }
    match stem {
        "RecentApplications" => Some(DataKind::RecentApplications),
        "RecentDocuments" => Some(DataKind::RecentDocuments),
        "RecentServers" | "RecentHosts" => Some(DataKind::RecentServers),
        _ => None,
    }
}

/// Folder inside `com.apple.sharedfilelist` with one `<bundle id>.sfl*` list per app.
const PER_APP_RECENT_DIR: &str = "com.apple.LSSharedFileList.ApplicationRecentDocuments";

fn scan_recent(env: &Env, cancel: &Cancel) -> Group {
    let dir = env.app_support("com.apple.sharedfilelist");
    let mut apps = Vec::new();
    let mut docs = Vec::new();
    let mut servers = Vec::new();
    let mut per_app = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            match classify_recent(&name) {
                Some(DataKind::RecentApplications) => apps.push(entry.path()),
                Some(DataKind::RecentDocuments) => docs.push(entry.path()),
                Some(DataKind::RecentServers) => servers.push(entry.path()),
                _ => {}
            }
        }
    }
    // macOS keeps one list per app in this folder (older systems used the short name).
    for folder in [PER_APP_RECENT_DIR, "ApplicationRecentDocuments"] {
        if let Ok(entries) = std::fs::read_dir(dir.join(folder)) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().contains(".sfl") {
                    per_app.push(entry.path());
                }
            }
        }
    }
    let id = "recent";
    Group {
        id: id.into(),
        name: "Recent items".into(),
        icon: "clock",
        detail: "Apple menu and Open Recent lists".into(),
        app_name: None,
        installed: true,
        running: false,
        items: vec![
            item(id, DataKind::RecentApplications, apps, false, cancel),
            item(id, DataKind::RecentDocuments, docs, false, cancel),
            item(id, DataKind::RecentServers, servers, false, cancel),
            item(id, DataKind::AppRecentDocuments, per_app, false, cancel),
        ],
    }
}

// MARK: - Full Disk Access and running browsers

/// Probe folders that macOS only lets a process with Full Disk Access list.
/// (World-readable files such as the system TCC.db prove nothing.)
pub fn probe_full_disk_access(home: &Path) -> Access {
    let probes = [
        "Library/Safari",
        "Library/Mail",
        "Library/Messages",
        "Library/Application Support/com.apple.TCC",
        "Library/Cookies",
    ];
    let mut refused = false;
    let mut readable = false;
    for rel in probes {
        match std::fs::read_dir(home.join(rel)) {
            Ok(mut entries) => {
                // An empty listing is inconclusive: TCC may filter silently.
                readable |= entries.next().is_some();
            }
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => refused = true,
            Err(_) => {}
        }
    }
    // Any refusal wins, so a folder that happens to be open never hides a
    // blocked one. This is the same rule the Settings screen uses.
    if refused {
        Access::Missing
    } else if readable {
        Access::Granted
    } else {
        Access::Unknown
    }
}

/// True when any executable lives inside a `<app>` bundle (e.g. `Google Chrome.app`),
/// which covers the main process and all its helpers.
///
/// App extensions (`*.appex`) are ignored: macOS hosts them inside other apps
/// (Safari's link extension runs for Messages), so one running does not mean
/// the browser itself is open.
pub fn is_running(app: &str, exes: &[PathBuf]) -> bool {
    exes.iter().any(|exe| {
        let mut in_app = false;
        for c in exe.components() {
            let name = c.as_os_str();
            if name.to_string_lossy().ends_with(".appex") {
                return false;
            }
            in_app |= name == app;
        }
        in_app
    })
}
