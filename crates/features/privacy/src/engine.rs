//! Privacy engine: finds installed browsers and the private data they keep
//! (cache, cookies, history, website data, form data), plus macOS "recent
//! items" lists, and removes selected data through `nera_core::fs_util`.
//!
//! Nothing here touches the UI. Every path is derived from an [`Env`] so the
//! unit tests run against fixture directories only.
//!
//! The browser table, install detection, the running-browser rule, sizing and
//! everything macOS-only (Safari, Full Disk Access, recent items) live in the
//! per-platform module (`engine/macos.rs`, `engine/windows.rs`); scanning a
//! Chromium or Firefox profile and cleaning are shared.
//!
//! What is never offered, on purpose:
//! - saved passwords (`Login Data`, `logins.json`, `key4.db`), bookmarks,
//!   extensions and their settings;
//! - Firefox history: it lives in `places.sqlite` next to the bookmarks, and
//!   removing a row from a database is not a file removal.

use nera_core::fs_util::{self, Cancel};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(target_os = "macos")]
pub use macos::{classify_recent, probe_full_disk_access, FULL_DISK_ACCESS_PANE};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

pub use platform::{is_running, Env};

// MARK: - Model

/// A kind of private data a group can hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DataKind {
    Cache,
    Cookies,
    History,
    WebsiteData,
    DownloadsHistory,
    FormData,
    RecentApplications,
    RecentDocuments,
    RecentServers,
    AppRecentDocuments,
}

impl DataKind {
    /// Stable key used inside item ids.
    pub fn key(self) -> &'static str {
        match self {
            DataKind::Cache => "cache",
            DataKind::Cookies => "cookies",
            DataKind::History => "history",
            DataKind::WebsiteData => "website-data",
            DataKind::DownloadsHistory => "downloads",
            DataKind::FormData => "forms",
            DataKind::RecentApplications => "recent-apps",
            DataKind::RecentDocuments => "recent-docs",
            DataKind::RecentServers => "recent-servers",
            DataKind::AppRecentDocuments => "recent-app-docs",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DataKind::Cache => "Cache",
            DataKind::Cookies => "Cookies",
            DataKind::History => "Browsing history",
            DataKind::WebsiteData => "Website data",
            DataKind::DownloadsHistory => "Download history",
            DataKind::FormData => "Saved form data",
            DataKind::RecentApplications => "Recent applications",
            DataKind::RecentDocuments => "Recent documents",
            DataKind::RecentServers => "Recent servers",
            DataKind::AppRecentDocuments => "Per-app recent documents",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            DataKind::Cache => "Downloaded pages and images. Sites load slower the first time",
            DataKind::Cookies => "Signs you out of websites and resets tracking",
            DataKind::History => "Visited pages, searches and favicons. Bookmarks stay",
            DataKind::WebsiteData => "Local storage, IndexedDB and offline data kept by sites",
            DataKind::DownloadsHistory => "The list of downloaded files, not the files themselves",
            DataKind::FormData => "Autofill entries, saved addresses and cards, and custom search engines. Passwords stay",
            DataKind::RecentApplications => "Apple menu, Recent Items, applications",
            DataKind::RecentDocuments => "Apple menu, Recent Items, documents",
            DataKind::RecentServers => "Apple menu, Recent Items, servers",
            DataKind::AppRecentDocuments => "File menu, Open Recent, in each app",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            DataKind::Cache => "hard-drive",
            DataKind::Cookies => "cookie",
            DataKind::History => "clock",
            DataKind::WebsiteData => "database",
            DataKind::DownloadsHistory => "download",
            DataKind::FormData => "text-cursor-input",
            DataKind::RecentApplications => "app-window",
            DataKind::RecentDocuments => "file-clock",
            DataKind::RecentServers => "server",
            DataKind::AppRecentDocuments => "folder-clock",
        }
    }

    /// Permanent kinds are always deleted: moving a history file to the Trash
    /// would keep the private data on disk. Only caches follow the user's
    /// "move to Trash" preference.
    pub fn permanent(self) -> bool {
        self != DataKind::Cache
    }
}

/// One file or folder that belongs to an [`Item`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub path: PathBuf,
    pub bytes: u64,
}

/// One selectable row: a kind of data in one group.
#[derive(Clone, Debug)]
pub struct Item {
    /// `"<group id>:<kind key>"`, stable across scans.
    pub id: String,
    pub kind: DataKind,
    pub targets: Vec<Target>,
    pub bytes: u64,
    /// Lives in a location macOS protects; unreadable without Full Disk Access.
    pub needs_full_disk_access: bool,
}

/// A browser, or the macOS recent-items lists.
#[derive(Clone, Debug)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub icon: &'static str,
    /// One-line detail, e.g. "3 profiles".
    pub detail: String,
    /// Bundle folder name used to detect a running browser (`None` = never running).
    pub app_name: Option<&'static str>,
    /// The app is installed (Safari always is). `false` = only leftover data remains.
    pub installed: bool,
    pub running: bool,
    pub items: Vec<Item>,
}

impl Group {
    pub fn bytes(&self) -> u64 {
        self.items.iter().map(|i| i.bytes).sum()
    }
}

/// Whether Nera Cleaner can read macOS-protected folders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Granted,
    /// A protected folder exists and listing it was refused.
    Missing,
    /// No protected folder exists to probe.
    Unknown,
}

#[derive(Clone, Debug)]
pub struct ScanResult {
    pub groups: Vec<Group>,
    pub full_disk_access: Access,
    pub cancelled: bool,
}

// MARK: - Browser catalogue

/// Where one Chromium-based browser keeps its data. The platform module
/// resolves every folder to an absolute path.
struct Chromium {
    /// Folder holding `Local State` and the profiles.
    support: PathBuf,
    /// Folder with one whole-cache sub-folder per profile (macOS `~/Library/Caches/<browser>`).
    cache_root: Option<PathBuf>,
    /// Whole folders that are pure cache.
    flat_caches: Vec<PathBuf>,
    /// Folder laid out like `support` that holds the profiles' cache
    /// sub-folders ([`CHROMIUM_CACHE`]) only (Opera on Windows).
    cache_mirror: Option<PathBuf>,
    /// The support folder is itself the only profile (Opera).
    root_is_profile: bool,
}

/// Where Firefox keeps its data.
struct Firefox {
    /// Folder holding `profiles.ini` and `Profiles/`.
    support: PathBuf,
    /// Folder holding each relative profile's cache under the same relative path.
    cache_root: PathBuf,
}

enum Engine {
    #[cfg(target_os = "macos")]
    Safari,
    Firefox(Firefox),
    Chromium(Chromium),
}

struct Spec {
    id: &'static str,
    name: &'static str,
    /// What identifies the installed / running app: the `<name>.app` folder
    /// name on macOS, the executable name on Windows.
    app: &'static str,
    icon: &'static str,
    engine: Engine,
}

/// Relative paths inside one Chromium profile folder, by kind.
const CHROMIUM_CACHE: &[&str] = &[
    "Cache",
    "Code Cache",
    "GPUCache",
    "DawnCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "GrShaderCache",
    "ShaderCache",
    "Media Cache",
    "Service Worker/CacheStorage",
    "Service Worker/ScriptCache",
];
const CHROMIUM_COOKIES: &[&str] = &[
    "Cookies",
    "Cookies-journal",
    "Network/Cookies",
    "Network/Cookies-journal",
    "Extension Cookies",
    "Extension Cookies-journal",
];
const CHROMIUM_HISTORY: &[&str] = &[
    "History",
    "History-journal",
    "Archived History",
    "Archived History-journal",
    "Top Sites",
    "Top Sites-journal",
    "Visited Links",
    "Favicons",
    "Favicons-journal",
    "Network Action Predictor",
    "Network Action Predictor-journal",
    "Shortcuts",
    "Shortcuts-journal",
];
const CHROMIUM_WEBSITE: &[&str] = &[
    "Local Storage",
    "IndexedDB",
    "Session Storage",
    "Service Worker/Database",
    "File System",
    "databases",
];
const CHROMIUM_FORMS: &[&str] = &["Web Data", "Web Data-journal"];
/// Folders directly under the Chromium support root that hold shader caches.
const CHROMIUM_ROOT_CACHE: &[&str] = &["ShaderCache", "GrShaderCache", "GraphiteDawnCache"];

// MARK: - Scan

/// Detect browsers and measure their data. `exes` are the executable paths of
/// running processes ([`running_executables`]); `progress` receives a short
/// "what is being measured" line.
pub fn scan(env: &Env, exes: &[PathBuf], cancel: &Cancel, progress: &dyn Fn(&str)) -> ScanResult {
    let fda = platform::full_disk_access(env);
    let mut groups = Vec::new();

    for spec in platform::specs(env) {
        if cancel.is_cancelled() {
            break;
        }
        progress(&format!("Looking for {}", spec.name));
        let app = env.find_app(spec.app);
        let built = match &spec.engine {
            #[cfg(target_os = "macos")]
            Engine::Safari => Some(platform::scan_safari(
                env,
                &spec,
                app.is_some(),
                cancel,
                progress,
            )),
            Engine::Firefox(ff) => scan_firefox(&spec, ff, app.is_some(), cancel, progress),
            Engine::Chromium(c) => scan_chromium(&spec, c, app.is_some(), cancel, progress),
        };
        // Safari ships with macOS and is listed even without data.
        #[cfg(target_os = "macos")]
        let built_in = matches!(spec.engine, Engine::Safari);
        #[cfg(not(target_os = "macos"))]
        let built_in = false;
        if let Some(mut group) = built {
            // A removed browser is listed only while it still leaves data behind.
            if app.is_none() && group.bytes() == 0 && !built_in {
                continue;
            }
            group.running = is_running(spec.app, exes);
            groups.push(group);
        }
    }

    if !cancel.is_cancelled() {
        groups.extend(platform::system_groups(env, cancel, progress));
    }

    ScanResult {
        groups,
        full_disk_access: fda,
        cancelled: cancel.is_cancelled(),
    }
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Measure every candidate that exists. Hard links shared between the
/// candidates are counted once, so the sum equals the space really used.
fn measure(candidates: Vec<PathBuf>, cancel: &Cancel) -> Vec<Target> {
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|p| exists(p))
        .map(|path| {
            let bytes = platform::unique_size(&path, &mut seen, cancel);
            Target { path, bytes }
        })
        .collect()
}

fn item(
    group: &str,
    kind: DataKind,
    candidates: Vec<PathBuf>,
    needs_fda: bool,
    cancel: &Cancel,
) -> Item {
    let targets = measure(candidates, cancel);
    let bytes = targets.iter().map(|t| t.bytes).sum();
    Item {
        id: format!("{group}:{}", kind.key()),
        kind,
        targets,
        bytes,
        needs_full_disk_access: needs_fda,
    }
}

fn scan_chromium(
    spec: &Spec,
    c: &Chromium,
    has_app: bool,
    cancel: &Cancel,
    progress: &dyn Fn(&str),
) -> Option<Group> {
    let support = c.support.clone();
    if !has_app && !support.is_dir() {
        return None;
    }
    let profiles = if c.root_is_profile {
        vec![Profile {
            dir: String::new(),
            name: "Default".into(),
        }]
    } else {
        chromium_profiles(&support)
    };

    let mut cache = Vec::new();
    let mut cookies = Vec::new();
    let mut history = Vec::new();
    let mut website = Vec::new();
    let mut forms = Vec::new();

    for profile in &profiles {
        if cancel.is_cancelled() {
            break;
        }
        progress(&format!("Measuring {} · {}", spec.name, profile.name));
        let base = if profile.dir.is_empty() {
            support.clone()
        } else {
            support.join(&profile.dir)
        };
        let join =
            |rels: &[&str], out: &mut Vec<PathBuf>| out.extend(rels.iter().map(|r| base.join(r)));
        join(CHROMIUM_CACHE, &mut cache);
        join(CHROMIUM_COOKIES, &mut cookies);
        join(CHROMIUM_HISTORY, &mut history);
        join(CHROMIUM_WEBSITE, &mut website);
        join(CHROMIUM_FORMS, &mut forms);
        if let (Some(root), false) = (&c.cache_root, profile.dir.is_empty()) {
            cache.push(root.join(&profile.dir));
        }
        if let Some(mirror) = &c.cache_mirror {
            let mirror = if profile.dir.is_empty() {
                mirror.clone()
            } else {
                mirror.join(&profile.dir)
            };
            cache.extend(CHROMIUM_CACHE.iter().map(|r| mirror.join(r)));
        }
    }
    if !c.root_is_profile {
        cache.extend(CHROMIUM_ROOT_CACHE.iter().map(|r| support.join(r)));
    }
    cache.extend(c.flat_caches.iter().cloned());

    let items = vec![
        item(spec.id, DataKind::Cache, cache, false, cancel),
        item(spec.id, DataKind::Cookies, cookies, false, cancel),
        item(spec.id, DataKind::History, history, false, cancel),
        item(spec.id, DataKind::WebsiteData, website, false, cancel),
        item(spec.id, DataKind::FormData, forms, false, cancel),
    ];
    let detail = match profiles.len() {
        0 => "No profiles yet".to_string(),
        1 => "1 profile".to_string(),
        n => format!("{n} profiles"),
    };
    Some(Group {
        id: spec.id.into(),
        name: spec.name.into(),
        icon: spec.icon,
        detail,
        app_name: Some(spec.app),
        installed: has_app,
        running: false,
        items,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    /// Folder name inside the support root ("Default", "Profile 2"); empty = the root itself.
    pub dir: String,
    pub name: String,
}

/// Chromium profiles: those named in `Local State` plus any `Default` /
/// `Profile N` folder that exists on disk.
pub fn chromium_profiles(support: &Path) -> Vec<Profile> {
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    if let Ok(bytes) = std::fs::read(support.join("Local State")) {
        if let Ok(json) = serde_json::from_slice::<Value>(&bytes) {
            if let Some(cache) = json
                .pointer("/profile/info_cache")
                .and_then(Value::as_object)
            {
                for (dir, info) in cache {
                    let name = info.get("name").and_then(Value::as_str).unwrap_or(dir);
                    found.insert(dir.clone(), name.to_string());
                }
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(support) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_profile = name == "Default"
                || name
                    .strip_prefix("Profile ")
                    .is_some_and(|n| n.parse::<u32>().is_ok());
            if is_profile && entry.path().is_dir() {
                found.entry(name.clone()).or_insert(name);
            }
        }
    }
    let mut profiles: Vec<Profile> = found
        .into_iter()
        .filter(|(dir, _)| support.join(dir).is_dir())
        .map(|(dir, name)| Profile { dir, name })
        .collect();
    // "Default" first, then Profile 1, 2, ... numerically.
    profiles.sort_by_key(|p| {
        let n = p
            .dir
            .strip_prefix("Profile ")
            .and_then(|n| n.parse::<u32>().ok());
        (p.dir != "Default", n.unwrap_or(0), p.dir.clone())
    });
    profiles
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirefoxProfile {
    /// Path as written in `profiles.ini`, relative to the Firefox folder ("Profiles/abc.default").
    pub rel: Option<String>,
    pub dir: PathBuf,
}

/// Files only a real Firefox profile folder has.
const FIREFOX_PROFILE_MARKERS: &[&str] = &["prefs.js", "times.json", "compatibility.ini"];

/// Firefox profiles: those in `profiles.ini` (`[ProfileN]` sections and
/// `[InstallXXXX] Default=`) plus every folder under `Profiles/` that looks
/// like one. Firefox's newer profile manager keeps its profiles in
/// `Profile Groups/*.sqlite` instead of `profiles.ini`, so `profiles.ini`
/// alone misses them. Only profiles that exist on disk are returned, each once.
pub fn firefox_profiles(firefox_dir: &Path) -> Vec<FirefoxProfile> {
    let mut out = firefox_profiles_from_ini(firefox_dir);
    if let Ok(entries) = std::fs::read_dir(firefox_dir.join("Profiles")) {
        let mut found: Vec<(String, PathBuf)> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter(|e| {
                FIREFOX_PROFILE_MARKERS
                    .iter()
                    .any(|m| e.path().join(m).exists())
            })
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
            .collect();
        found.sort();
        for (name, dir) in found {
            if !out.iter().any(|p| p.dir == dir) {
                out.push(FirefoxProfile {
                    rel: Some(format!("Profiles/{name}")),
                    dir,
                });
            }
        }
    }
    out
}

fn firefox_profiles_from_ini(firefox_dir: &Path) -> Vec<FirefoxProfile> {
    let Ok(text) = std::fs::read_to_string(firefox_dir.join("profiles.ini")) else {
        return Vec::new();
    };
    let mut out: Vec<FirefoxProfile> = Vec::new();
    let mut push = |path: &str, relative: bool| {
        if path.is_empty() {
            return;
        }
        let (dir, rel) = if relative {
            (firefox_dir.join(path), Some(path.to_string()))
        } else {
            (PathBuf::from(path), None)
        };
        if dir.is_dir() && !out.iter().any(|p| p.dir == dir) {
            out.push(FirefoxProfile { rel, dir });
        }
    };

    let mut section = String::new();
    let mut path = String::new();
    let mut relative = true;
    let flush = |section: &str,
                 path: &mut String,
                 relative: &mut bool,
                 push: &mut dyn FnMut(&str, bool)| {
        if section.starts_with("Profile") {
            push(path, *relative);
        }
        path.clear();
        *relative = true;
    };
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            flush(&section, &mut path, &mut relative, &mut push);
            section = name.to_string();
        } else if let Some((key, value)) = line.split_once('=') {
            match (section.as_str(), key.trim()) {
                (s, "Path") if s.starts_with("Profile") => path = value.trim().to_string(),
                (s, "IsRelative") if s.starts_with("Profile") => relative = value.trim() != "0",
                (s, "Default") if s.starts_with("Install") => push(value.trim(), true),
                _ => {}
            }
        }
    }
    flush(&section, &mut path, &mut relative, &mut push);
    out
}

fn scan_firefox(
    spec: &Spec,
    ff: &Firefox,
    has_app: bool,
    cancel: &Cancel,
    progress: &dyn Fn(&str),
) -> Option<Group> {
    let support = ff.support.clone();
    if !has_app && !support.is_dir() {
        return None;
    }
    let profiles = firefox_profiles(&support);
    let mut cache = Vec::new();
    let mut cookies = Vec::new();
    let mut website = Vec::new();
    let mut forms = Vec::new();
    let sidecars = |dir: &Path, name: &str| -> Vec<PathBuf> {
        ["", "-wal", "-shm", "-journal"]
            .iter()
            .map(|s| dir.join(format!("{name}{s}")))
            .collect()
    };
    for profile in &profiles {
        if cancel.is_cancelled() {
            break;
        }
        progress(&format!(
            "Measuring {} · {}",
            spec.name,
            profile
                .dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        ));
        if let Some(rel) = &profile.rel {
            cache.push(ff.cache_root.join(rel));
        }
        cookies.extend(sidecars(&profile.dir, "cookies.sqlite"));
        forms.extend(sidecars(&profile.dir, "formhistory.sqlite"));
        // Per-site storage. Extension storage (`moz-extension+++...`) is kept.
        let default_storage = profile.dir.join("storage/default");
        if let Ok(entries) = std::fs::read_dir(&default_storage) {
            for entry in entries.flatten() {
                if !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("moz-extension")
                {
                    website.push(entry.path());
                }
            }
        }
        website.push(profile.dir.join("storage/temporary"));
    }
    let items = vec![
        item(spec.id, DataKind::Cache, cache, false, cancel),
        item(spec.id, DataKind::Cookies, cookies, false, cancel),
        item(spec.id, DataKind::WebsiteData, website, false, cancel),
        item(spec.id, DataKind::FormData, forms, false, cancel),
    ];
    let detail = match profiles.len() {
        0 => "No profiles yet".to_string(),
        1 => "1 profile".to_string(),
        n => format!("{n} profiles"),
    };
    Some(Group {
        id: spec.id.into(),
        name: spec.name.into(),
        icon: spec.icon,
        detail,
        app_name: Some(spec.app),
        installed: has_app,
        running: false,
        items,
    })
}

// MARK: - Running browsers

/// Executable paths of every process this user can see.
pub fn running_executables() -> Vec<PathBuf> {
    fs_util::all_pids()
        .into_iter()
        .filter_map(fs_util::executable_path)
        .collect()
}

// MARK: - Clean

/// What to remove for one selected [`Item`].
#[derive(Clone, Debug)]
pub struct CleanItem {
    pub id: String,
    pub group_name: String,
    pub app_name: Option<&'static str>,
    pub kind: DataKind,
    pub targets: Vec<Target>,
}

impl CleanItem {
    pub fn from_item(group: &Group, item: &Item) -> Self {
        Self {
            id: item.id.clone(),
            group_name: group.name.clone(),
            app_name: group.app_name,
            kind: item.kind,
            targets: item.targets.clone(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CleanReport {
    /// Ids of items whose every target was removed (or already gone).
    pub cleaned: Vec<String>,
    /// Bytes deleted permanently: space that is free again.
    pub freed: u64,
    /// Bytes moved to the Trash: still on disk until the Trash is emptied.
    pub trashed_bytes: u64,
    /// Number of files/folders removed.
    pub removed: usize,
    /// How many of those went to the Trash instead of being deleted.
    pub trashed: usize,
    pub failed: Vec<(PathBuf, String)>,
    /// Names of browsers that were running; their items were left alone.
    pub skipped_running: Vec<String>,
    pub cancelled: bool,
}

/// Remove the selected data. Browsers found in `exes` are skipped: their
/// databases are open and would be corrupted or restored. Caches follow
/// `to_trash`; every other kind is deleted permanently.
pub fn clean(
    items: &[CleanItem],
    exes: &[PathBuf],
    to_trash: bool,
    cancel: &Cancel,
    progress: &dyn Fn(&str),
) -> CleanReport {
    let mut report = CleanReport::default();
    for item in items {
        if cancel.is_cancelled() {
            report.cancelled = true;
            break;
        }
        if item.app_name.is_some_and(|app| is_running(app, exes)) {
            if !report.skipped_running.contains(&item.group_name) {
                report.skipped_running.push(item.group_name.clone());
            }
            continue;
        }
        progress(&format!("{} · {}", item.group_name, item.kind.label()));
        let mut ok = true;
        for target in &item.targets {
            if cancel.is_cancelled() {
                report.cancelled = true;
                ok = false;
                break;
            }
            if !exists(&target.path) {
                continue;
            }
            let use_trash = to_trash && !item.kind.permanent();
            let result = if item.kind.permanent() {
                fs_util::remove_item(&target.path)
            } else {
                fs_util::remove_or_trash(&target.path, to_trash)
            };
            match result {
                Ok(()) => {
                    report.removed += 1;
                    if use_trash {
                        report.trashed += 1;
                        report.trashed_bytes += target.bytes;
                    } else {
                        report.freed += target.bytes;
                    }
                }
                Err(err) => {
                    ok = false;
                    report.failed.push((target.path.clone(), err.to_string()));
                }
            }
        }
        if ok {
            report.cleaned.push(item.id.clone());
        }
    }
    report
}

// MARK: - Tests

/// macOS layout and behaviour. The Windows tests live in `engine/windows.rs`.
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::fs;

    struct Fixture {
        root: PathBuf,
        env: Env,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("nera-privacy-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            let home = root.join("home");
            let apps = root.join("Applications");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&apps).unwrap();
            Self {
                env: Env {
                    home,
                    app_dirs: vec![apps],
                },
                root,
            }
        }

        fn app(&self, name: &str) {
            fs::create_dir_all(self.root.join("Applications").join(name)).unwrap();
        }

        fn file(&self, rel: &str, len: usize) -> PathBuf {
            let path = self.env.home.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, vec![7u8; len]).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn scan_fixture(f: &Fixture, exes: &[PathBuf]) -> ScanResult {
        scan(&f.env, exes, &Cancel::new(), &|_| {})
    }

    fn group<'a>(r: &'a ScanResult, id: &str) -> &'a Group {
        r.groups
            .iter()
            .find(|g| g.id == id)
            .unwrap_or_else(|| panic!("no group {id}"))
    }

    fn item_of(g: &Group, kind: DataKind) -> &Item {
        g.items
            .iter()
            .find(|i| i.kind == kind)
            .unwrap_or_else(|| panic!("no item {kind:?}"))
    }

    const CHROME: &str = "Library/Application Support/Google/Chrome";

    fn chrome_fixture(tag: &str) -> Fixture {
        let f = Fixture::new(tag);
        f.app("Google Chrome.app");
        f.file(&format!("{CHROME}/Local State"), 0);
        fs::write(
            f.env.home.join(CHROME).join("Local State"),
            r#"{"profile":{"info_cache":{"Default":{"name":"Person 1"},"Profile 2":{"name":"Work"},"Profile 9":{"name":"Gone"}}}}"#,
        )
        .unwrap();
        for p in ["Default", "Profile 2"] {
            f.file(&format!("{CHROME}/{p}/Cookies"), 5000);
            f.file(&format!("{CHROME}/{p}/Cookies-journal"), 500);
            f.file(&format!("{CHROME}/{p}/History"), 8000);
            f.file(
                &format!("{CHROME}/{p}/Local Storage/leveldb/000003.log"),
                4000,
            );
            f.file(&format!("{CHROME}/{p}/Web Data"), 3000);
            f.file(
                &format!("{CHROME}/{p}/Service Worker/CacheStorage/a/b"),
                6000,
            );
            f.file(
                &format!("{CHROME}/{p}/Service Worker/Database/000003.log"),
                2000,
            );
            // Never offered.
            f.file(&format!("{CHROME}/{p}/Login Data"), 9000);
            f.file(&format!("{CHROME}/{p}/Bookmarks"), 1000);
            f.file(&format!("{CHROME}/{p}/Extensions/ext/manifest.json"), 1000);
            f.file(
                &format!("{CHROME}/{p}/Local Extension Settings/ext/x"),
                1000,
            );
        }
        f.file("Library/Caches/Google/Chrome/Default/Cache/data_0", 12_000);
        f.file(
            "Library/Caches/Google/Chrome/Profile 2/Cache/data_0",
            12_000,
        );
        f
    }

    #[test]
    fn chromium_profiles_come_from_local_state_and_disk() {
        let f = chrome_fixture("profiles");
        let profiles = chromium_profiles(&f.env.home.join(CHROME));
        let dirs: Vec<&str> = profiles.iter().map(|p| p.dir.as_str()).collect();
        // "Profile 9" is in Local State but has no folder; "Local State" itself is not a profile.
        assert_eq!(dirs, ["Default", "Profile 2"]);
        assert_eq!(profiles[1].name, "Work");
    }

    #[test]
    fn scans_chrome_across_profiles() {
        let f = chrome_fixture("scan-chrome");
        let r = scan_fixture(&f, &[]);
        let g = group(&r, "chrome");
        assert_eq!(g.detail, "2 profiles");
        assert!(!g.running);
        assert!(g.installed);

        let cookies = item_of(g, DataKind::Cookies);
        assert_eq!(cookies.id, "chrome:cookies");
        assert_eq!(cookies.targets.len(), 4, "two files in two profiles");
        assert!(cookies.bytes >= 2 * 5500);

        let cache = item_of(g, DataKind::Cache);
        let paths: Vec<_> = cache.targets.iter().map(|t| t.path.clone()).collect();
        assert!(paths
            .iter()
            .any(|p| p.ends_with("Profile 2/Service Worker/CacheStorage")));
        assert!(paths
            .iter()
            .any(|p| p.ends_with("Caches/Google/Chrome/Profile 2")));
        assert!(item_of(g, DataKind::History).bytes >= 16_000);
        assert!(item_of(g, DataKind::WebsiteData).bytes >= 12_000);
        assert!(item_of(g, DataKind::FormData).bytes >= 6000);

        // Passwords, bookmarks and extension data are never targets.
        let all: Vec<String> = g
            .items
            .iter()
            .flat_map(|i| i.targets.iter())
            .map(|t| t.path.to_string_lossy().into_owned())
            .collect();
        for forbidden in [
            "Login Data",
            "Bookmarks",
            "Extensions",
            "Local Extension Settings",
        ] {
            assert!(
                all.iter().all(|p| !p.contains(forbidden)),
                "{forbidden} must not be offered"
            );
        }
    }

    #[test]
    fn uninstalled_browsers_are_not_listed_but_safari_and_recent_are() {
        let f = Fixture::new("none");
        let r = scan_fixture(&f, &[]);
        let ids: Vec<&str> = r.groups.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids, ["safari", "recent"]);
        assert!(r.groups.iter().all(|g| g.bytes() == 0));
    }

    #[test]
    fn leftover_support_data_lists_a_removed_browser() {
        let f = Fixture::new("leftover");
        f.file(
            "Library/Application Support/BraveSoftware/Brave-Browser/Default/Cookies",
            3000,
        );
        let r = scan_fixture(&f, &[]);
        assert!(item_of(group(&r, "brave"), DataKind::Cookies).bytes >= 3000);
        assert!(!group(&r, "brave").installed);
    }

    #[test]
    fn running_browser_is_flagged_by_bundle_component() {
        let f = chrome_fixture("running");
        let exes = vec![
            PathBuf::from("/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Framework.framework/Helpers/Google Chrome Helper.app/Contents/MacOS/Google Chrome Helper"),
            PathBuf::from("/usr/bin/vim"),
        ];
        let r = scan_fixture(&f, &exes);
        assert!(group(&r, "chrome").running);
        assert!(!group(&r, "safari").running);
        assert!(!is_running("Firefox.app", &exes));
        assert!(is_running("Safari.app", &[PathBuf::from("/System/Volumes/Preboot/Cryptexes/App/System/Applications/Safari.app/Contents/MacOS/Safari")]));
    }

    #[test]
    fn firefox_profiles_come_from_profiles_ini_without_duplicates() {
        let f = Fixture::new("firefox");
        f.app("Firefox.app");
        let ff = "Library/Application Support/Firefox";
        for p in ["Profiles/aaa.default", "Profiles/bbb.default-release"] {
            f.file(&format!("{ff}/{p}/cookies.sqlite"), 4000);
            f.file(&format!("{ff}/{p}/cookies.sqlite-wal"), 1000);
            f.file(&format!("{ff}/{p}/formhistory.sqlite"), 2000);
            f.file(&format!("{ff}/{p}/places.sqlite"), 9000);
            f.file(&format!("{ff}/{p}/logins.json"), 9000);
            f.file(
                &format!("{ff}/{p}/storage/default/https+++example.com/ls/data.sqlite"),
                3000,
            );
            f.file(
                &format!("{ff}/{p}/storage/default/moz-extension+++abc/idb/x"),
                3000,
            );
        }
        f.file(
            "Library/Caches/Firefox/Profiles/bbb.default-release/cache2/entries/e1",
            7000,
        );
        fs::write(
            f.env.home.join(ff).join("profiles.ini"),
            "[Profile1]\nName=default\nIsRelative=1\nPath=Profiles/aaa.default\nDefault=1\n\n\
             [Profile0]\nName=default-release\nIsRelative=1\nPath=Profiles/bbb.default-release\n\n\
             [Profile2]\nName=ghost\nIsRelative=1\nPath=Profiles/missing\n\n\
             [InstallABC]\nDefault=Profiles/bbb.default-release\nLocked=1\n\n[General]\nVersion=2\n",
        )
        .unwrap();

        let profiles = firefox_profiles(&f.env.home.join(ff));
        assert_eq!(
            profiles.len(),
            2,
            "ghost profile skipped, install default deduplicated"
        );

        // A profile made by the newer profile manager is not in profiles.ini.
        f.file(&format!("{ff}/Profiles/ccc.Profile 1/prefs.js"), 10);
        f.file(&format!("{ff}/Profiles/ccc.Profile 1/cookies.sqlite"), 4000);
        f.file(&format!("{ff}/Profiles/not-a-profile/readme.txt"), 10);
        let profiles = firefox_profiles(&f.env.home.join(ff));
        assert_eq!(profiles.len(), 3);
        assert_eq!(profiles[2].rel.as_deref(), Some("Profiles/ccc.Profile 1"));
        fs::remove_dir_all(f.env.home.join(ff).join("Profiles/ccc.Profile 1")).unwrap();
        fs::remove_dir_all(f.env.home.join(ff).join("Profiles/not-a-profile")).unwrap();

        let r = scan_fixture(&f, &[]);
        let g = group(&r, "firefox");
        assert_eq!(g.detail, "2 profiles");
        assert!(item_of(g, DataKind::Cache).bytes >= 7000);
        assert_eq!(item_of(g, DataKind::Cookies).targets.len(), 4);
        assert!(
            g.items.iter().all(|i| i.kind != DataKind::History),
            "Firefox history shares a file with bookmarks"
        );
        let site_targets: Vec<String> = item_of(g, DataKind::WebsiteData)
            .targets
            .iter()
            .map(|t| t.path.to_string_lossy().into_owned())
            .collect();
        assert!(site_targets
            .iter()
            .any(|p| p.contains("https+++example.com")));
        assert!(site_targets.iter().all(|p| !p.contains("moz-extension")));
    }

    #[test]
    fn safari_items_need_full_disk_access() {
        let f = Fixture::new("safari");
        f.file("Library/Caches/com.apple.Safari/Cache.db", 5000);
        f.file("Library/Safari/History.db", 4000);
        f.file("Library/Safari/History.db-wal", 1000);
        f.file("Library/Safari/Downloads.plist", 500);
        f.file("Library/Safari/Bookmarks.plist", 500);
        let r = scan_fixture(&f, &[]);
        let g = group(&r, "safari");
        // The cache is split between ~/Library/Caches and Safari's container.
        assert!(item_of(g, DataKind::Cache).needs_full_disk_access);
        assert!(item_of(g, DataKind::History).needs_full_disk_access);
        assert_eq!(item_of(g, DataKind::History).targets.len(), 2);
        assert!(item_of(g, DataKind::DownloadsHistory).bytes >= 500);
        let any_bookmarks = g
            .items
            .iter()
            .flat_map(|i| i.targets.iter())
            .any(|t| t.path.ends_with("Bookmarks.plist"));
        assert!(!any_bookmarks);
    }

    #[test]
    fn only_recent_lists_are_classified() {
        use DataKind::*;
        assert_eq!(
            classify_recent("com.apple.LSSharedFileList.RecentApplications.sfl4"),
            Some(RecentApplications)
        );
        assert_eq!(
            classify_recent("com.apple.LSSharedFileList.RecentDocuments.sfl2"),
            Some(RecentDocuments)
        );
        assert_eq!(
            classify_recent("com.apple.LSSharedFileList.RecentServers.sfl3"),
            Some(RecentServers)
        );
        for keep in [
            "com.apple.LSSharedFileList.FavoriteItems.sfl4",
            "com.apple.LSSharedFileList.FavoriteVolumes.sfl3",
            "com.apple.LSSharedFileList.iCloudItems.sfl4",
            "com.apple.LSSharedFileList.ProjectsItems.sfl4",
            "com.apple.LSSharedFileList.ApplicationRecentDocuments",
            "com.apple.LSSharedFileList.RecentDocuments",
        ] {
            assert_eq!(classify_recent(keep), None, "{keep}");
        }
    }

    #[test]
    fn scans_recent_items() {
        let f = Fixture::new("recent");
        let sfl = "Library/Application Support/com.apple.sharedfilelist";
        f.file(
            &format!("{sfl}/com.apple.LSSharedFileList.RecentApplications.sfl4"),
            100,
        );
        f.file(
            &format!("{sfl}/com.apple.LSSharedFileList.RecentDocuments.sfl4"),
            200,
        );
        f.file(
            &format!("{sfl}/com.apple.LSSharedFileList.FavoriteItems.sfl4"),
            300,
        );
        f.file(&format!("{sfl}/com.apple.LSSharedFileList.ApplicationRecentDocuments/com.apple.preview.sfl4"), 400);
        f.file(&format!("{sfl}/com.apple.LSSharedFileList.ApplicationRecentDocuments/com.apple.textedit.sfl3"), 400);
        f.file(
            &format!("{sfl}/com.apple.LSSharedFileList.ApplicationRecentDocuments/notes.txt"),
            400,
        );
        let r = scan_fixture(&f, &[]);
        let g = group(&r, "recent");
        assert_eq!(
            item_of(g, DataKind::AppRecentDocuments).targets.len(),
            2,
            "only .sfl lists, from the real folder"
        );
        assert_eq!(item_of(g, DataKind::RecentApplications).targets.len(), 1);
        assert_eq!(item_of(g, DataKind::RecentDocuments).targets.len(), 1);
        let all: Vec<_> = g.items.iter().flat_map(|i| i.targets.iter()).collect();
        assert!(all
            .iter()
            .all(|t| !t.path.to_string_lossy().contains("Favorite")));
    }

    #[test]
    fn clean_removes_selected_data_and_keeps_everything_else() {
        let f = chrome_fixture("clean");
        let r = scan_fixture(&f, &[]);
        let g = group(&r, "chrome");
        let selected: Vec<CleanItem> = g.items.iter().map(|i| CleanItem::from_item(g, i)).collect();
        let expect_freed: u64 = g.bytes();

        let report = clean(&selected, &[], false, &Cancel::new(), &|_| {});
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert!(report.skipped_running.is_empty());
        assert_eq!(report.freed, expect_freed);
        assert_eq!(report.trashed, 0);
        assert_eq!(report.cleaned.len(), g.items.len());

        let home = &f.env.home;
        for p in ["Default", "Profile 2"] {
            for gone in [
                "Cookies",
                "History",
                "Web Data",
                "Local Storage",
                "Service Worker/CacheStorage",
            ] {
                assert!(
                    !home.join(CHROME).join(p).join(gone).exists(),
                    "{p}/{gone} should be removed"
                );
            }
            for kept in [
                "Login Data",
                "Bookmarks",
                "Extensions",
                "Local Extension Settings",
                "Service Worker",
            ] {
                assert!(
                    home.join(CHROME).join(p).join(kept).exists(),
                    "{p}/{kept} must survive"
                );
            }
        }
        assert!(!home.join("Library/Caches/Google/Chrome/Profile 2").exists());

        let again = scan_fixture(&f, &[]);
        assert_eq!(group(&again, "chrome").bytes(), 0);
    }

    #[test]
    fn clean_skips_running_browsers_without_touching_them() {
        let f = chrome_fixture("clean-running");
        let r = scan_fixture(&f, &[]);
        let g = group(&r, "chrome");
        let selected: Vec<CleanItem> = g.items.iter().map(|i| CleanItem::from_item(g, i)).collect();
        let exes = vec![PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        )];

        let report = clean(&selected, &exes, false, &Cancel::new(), &|_| {});
        assert_eq!(report.skipped_running, ["Google Chrome"]);
        assert_eq!(report.removed, 0);
        assert!(report.cleaned.is_empty());
        assert!(f.env.home.join(CHROME).join("Default/Cookies").exists());
    }

    #[test]
    fn clean_reports_failures_and_stops_when_cancelled() {
        let f = Fixture::new("clean-fail");
        let file = f.file("Library/Application Support/com.apple.sharedfilelist/com.apple.LSSharedFileList.RecentDocuments.sfl4", 100);
        // Protected locations are refused by nera_core::safety and reported, not skipped silently.
        let protected = CleanItem {
            id: "recent:recent-docs".into(),
            group_name: "Recent items".into(),
            app_name: None,
            kind: DataKind::RecentDocuments,
            targets: vec![Target {
                path: PathBuf::from("/System/Library"),
                bytes: 1,
            }],
        };
        let report = clean(&[protected], &[], false, &Cancel::new(), &|_| {});
        assert_eq!(report.failed.len(), 1);
        assert!(report.cleaned.is_empty());

        let cancel = Cancel::new();
        cancel.cancel();
        let item = CleanItem {
            id: "x".into(),
            group_name: "Recent items".into(),
            app_name: None,
            kind: DataKind::RecentDocuments,
            targets: vec![Target {
                path: file.clone(),
                bytes: 100,
            }],
        };
        let report = clean(&[item], &[], false, &cancel, &|_| {});
        assert!(report.cancelled);
        assert!(file.exists());
    }

    #[test]
    fn hard_links_are_counted_once_like_du() {
        let f = Fixture::new("hardlinks");
        let a = f.file(
            "Library/Caches/com.apple.Safari/WebKitCache/Records/a",
            40_000,
        );
        let blobs = f
            .env
            .home
            .join("Library/Caches/com.apple.Safari/WebKitCache/Blobs");
        fs::create_dir_all(&blobs).unwrap();
        fs::hard_link(&a, blobs.join("a")).unwrap();
        f.file(
            "Library/Caches/com.apple.Safari/WebKitCache/Records/b",
            20_000,
        );
        let r = scan_fixture(&f, &[]);
        let cache = item_of(group(&r, "safari"), DataKind::Cache);
        let single = fs_util::allocated_size(&fs::metadata(&a).unwrap());
        let other = fs_util::allocated_size(
            &fs::metadata(
                f.env
                    .home
                    .join("Library/Caches/com.apple.Safari/WebKitCache/Records/b"),
            )
            .unwrap(),
        );
        assert_eq!(
            cache.bytes,
            single + other,
            "the second link to the same file adds nothing"
        );
    }

    #[test]
    fn only_caches_follow_the_trash_preference() {
        assert!(!DataKind::Cache.permanent());
        for kind in [
            DataKind::Cookies,
            DataKind::History,
            DataKind::WebsiteData,
            DataKind::FormData,
            DataKind::RecentDocuments,
        ] {
            assert!(kind.permanent(), "{kind:?}");
        }
    }

    #[test]
    fn full_disk_access_probe() {
        use std::os::unix::fs::PermissionsExt;
        // Nothing to probe.
        let f = Fixture::new("fda-none");
        assert_eq!(probe_full_disk_access(&f.env.home), Access::Unknown);
        // A listable protected folder with content.
        f.file("Library/Safari/Bookmarks.plist", 10);
        assert_eq!(probe_full_disk_access(&f.env.home), Access::Granted);
        // A refused listing.
        let g = Fixture::new("fda-denied");
        g.file("Library/Safari/Bookmarks.plist", 10);
        let dir = g.env.home.join("Library/Safari");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
        let result = probe_full_disk_access(&g.env.home);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(result, Access::Missing);
        // One blocked folder is enough, even when another protected folder lists fine.
        let h = Fixture::new("fda-mixed");
        h.file("Library/Safari/Bookmarks.plist", 10);
        h.file("Library/Messages/chat.db", 10);
        let blocked = h.env.home.join("Library/Safari");
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
        let mixed = probe_full_disk_access(&h.env.home);
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(mixed, Access::Missing);
    }

    #[test]
    fn app_extensions_do_not_count_as_the_browser_running() {
        let safari =
            "/System/Volumes/Preboot/Cryptexes/App/System/Applications/Safari.app/Contents";
        let ext = PathBuf::from(format!(
            "{safari}/Extensions/SafariLinkExtension.appex/Contents/MacOS/SafariLinkExtension"
        ));
        assert!(!is_running("Safari.app", std::slice::from_ref(&ext)));
        let main = PathBuf::from(format!("{safari}/MacOS/Safari"));
        assert!(is_running("Safari.app", &[ext, main]));
        let xpc = PathBuf::from(format!(
            "{safari}/XPCServices/com.apple.Safari.SandboxBroker.xpc/Contents/MacOS/x"
        ));
        assert!(is_running("Safari.app", &[xpc]));
    }

    /// Read-only smoke test against this Mac: `cargo test -p nera-privacy -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_machine_scan_is_read_only() {
        let r = scan(
            &Env::real(),
            &running_executables(),
            &Cancel::new(),
            &|_| {},
        );
        println!("full disk access: {:?}", r.full_disk_access);
        for g in &r.groups {
            println!(
                "{} ({}) running={} {}",
                g.name,
                g.detail,
                g.running,
                nera_core::format::bytes(g.bytes())
            );
            for i in &g.items {
                println!(
                    "    {:<28} {:>10} targets={}",
                    i.kind.label(),
                    nera_core::format::bytes(i.bytes),
                    i.targets.len()
                );
            }
        }
    }
}
