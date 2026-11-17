//! Last line of defence for every delete/trash path.
//!
//! Feature code must never call `std::fs::remove_*` or move to Trash directly;
//! use [`crate::fs_util::remove_item`] / [`crate::fs_util::trash_item`], which
//! refuse anything [`is_deletable`] / [`is_trashable`] rejects.
//!
//! Besides system roots, this protects live state of running software: the
//! JetBrains/Android Studio "system" directories hold index files, LocalHistory,
//! the running IDE's `.pid`/`.port` and Toolbox sockets. Removing them while the
//! IDE runs corrupts it until it is reinstalled.

#[cfg(target_os = "macos")]
use crate::paths::BUNDLE_ID;
use crate::paths::{home, is_under, standardize};
use std::path::{Path, PathBuf};

/// Roots that must never be deleted OR trashed as a whole.
pub fn is_trashable(path: &Path) -> bool {
    let home = standardize(home());
    path.is_absolute()
        && forms(path).iter().all(|p| {
            !is_system_location(p, &home)
                && !holds_vm_disk(p, &home)
                && !is_sync_state(p, &home)
                && !is_live_app_state(p)
        })
}

/// Stricter check for permanent deletion of an individual item.
pub fn is_deletable(path: &Path) -> bool {
    if !is_trashable(path) {
        return false;
    }
    let home = standardize(home());
    forms(path)
        .iter()
        .all(|p| !is_vm_home(p, &home) && !user_folders(&home).iter().any(|folder| same(p, folder)))
}

/// Every spelling of `path` the rules are checked against: as written, and
/// with the folders above it resolved, so a symlinked ancestor or an alias
/// of the startup disk (`/Volumes/Macintosh HD/Users/me`) cannot reach a
/// protected location. The item itself is not resolved: removing a symlink
/// removes the link, not its target.
fn forms(path: &Path) -> Vec<PathBuf> {
    let mut forms = vec![standardize(path)];
    if let Some(resolved) = resolve_ancestors(path).map(|p| standardize(&p)) {
        if resolved != forms[0] {
            forms.push(resolved);
        }
    }
    forms
}

#[cfg(target_os = "macos")]
fn resolve_ancestors(path: &Path) -> Option<PathBuf> {
    let parent = std::fs::canonicalize(path.parent()?).ok()?;
    Some(parent.join(path.file_name()?))
}

/// Windows: `canonicalize` returns verbatim (`\\?\`) paths the rules do not
/// parse; junctions are handled where folders are walked.
#[cfg(not(target_os = "macos"))]
fn resolve_ancestors(_path: &Path) -> Option<PathBuf> {
    None
}

fn same(a: &Path, b: &Path) -> bool {
    crate::sys::starts_with(a, b) && crate::sys::starts_with(b, a)
}

// MARK: - macOS locations

/// System roots and everything the OS owns.
#[cfg(target_os = "macos")]
fn is_system_location(p: &Path, home: &Path) -> bool {
    let exact: [&Path; 10] = [
        Path::new("/"),
        Path::new("/Users"),
        Path::new("/Applications"),
        Path::new("/Library"),
        Path::new("/System"),
        Path::new("/var"),
        Path::new("/etc"),
        Path::new("/tmp"),
        Path::new("/cores"),
        Path::new("/private"),
    ];
    if exact.contains(&p) || p == home {
        return true;
    }
    let prefixes = [
        "/System",
        "/usr",
        "/bin",
        "/sbin",
        "/etc",
        "/opt",
        "/var/vm",
        "/var/db",
        "/Library/Apple",
    ];
    prefixes.iter().any(|pre| p.starts_with(pre))
}

/// Container VM disks: only `docker system prune` / OrbStack may touch
/// them, so neither the disk nor a folder holding it may go to the Trash.
#[cfg(target_os = "macos")]
fn holds_vm_disk(p: &Path, home: &Path) -> bool {
    let vm_homes = [
        home.join("Library/Containers/com.docker.docker"),
        home.join(".docker"),
        home.join(".orbstack"),
    ];
    vm_homes
        .iter()
        .any(|v| v.starts_with(p) || (p.starts_with(v) && p.to_string_lossy().ends_with(".raw")))
}

/// State of cloud-drive clients. Google Drive keeps files that are not
/// uploaded yet in its `DriveFS` folder (it looks like a large cache), and
/// removing a drive's folder in `CloudStorage` deletes the drive's contents
/// everywhere. Files inside a drive stay removable: that is the user's call.
#[cfg(target_os = "macos")]
fn is_sync_state(p: &Path, home: &Path) -> bool {
    let drive_fs = home.join("Library/Application Support/Google/DriveFS");
    if p.starts_with(&drive_fs) || drive_fs.starts_with(p) {
        return true;
    }
    let cloud = home.join("Library/CloudStorage");
    p == cloud || p.parent() == Some(cloud.as_path())
}

#[cfg(windows)]
fn is_sync_state(_p: &Path, _home: &Path) -> bool {
    false
}

/// VM state that may be trashed piecemeal but never deleted outright.
#[cfg(target_os = "macos")]
fn is_vm_home(p: &Path, home: &Path) -> bool {
    // Docker's raw VM disk: only `docker system prune` may touch it.
    let s = p.to_string_lossy();
    if s.contains("/com.docker.docker") && s.ends_with(".raw") {
        return true;
    }
    p == home.join(".docker") || p == home.join(".orbstack")
}

/// User folders that are never deleted as a whole (their contents may be).
#[cfg(target_os = "macos")]
fn user_folders(home: &Path) -> Vec<PathBuf> {
    [
        "Library",
        "Documents",
        "Desktop",
        "Downloads",
        "Pictures",
        "Movies",
        "Music",
        "Library/Application Support",
        "Library/Containers",
        "Library/Group Containers",
        "Library/Caches",
        "Library/Logs",
        "Library/Preferences",
        "Library/Keychains",
        "Library/Mail",
        "Library/Messages",
        "Library/Mobile Documents",
    ]
    .iter()
    .map(|rel| home.join(rel))
    .collect()
}

/// Nera Cleaner's own cache folders.
#[cfg(target_os = "macos")]
fn is_own_state(p: &Path, home: &Path) -> bool {
    let caches = home.join("Library/Caches");
    is_under(p, caches.join(BUNDLE_ID)) || is_under(p, caches.join("io.sentry"))
}

// MARK: - Windows locations

/// Drive roots, Windows itself, installed programs and other users' profiles.
/// Only the contents of `Windows\Temp` are removable below the Windows folder.
#[cfg(windows)]
fn is_system_location(p: &Path, home: &Path) -> bool {
    use crate::sys::starts_with;
    use std::path::Component;
    let names: Vec<String> = p
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    let Some(top) = names.first() else {
        return true; // a drive or share root
    };
    if same(p, home) || home.parent().is_some_and(|users| starts_with(users, p)) {
        return true; // the profile, `C:\Users`, or anything above them
    }
    if names.len() == 1 {
        // Page, hibernation and swap files, and the root folders Windows owns.
        let owned = ["users", "programdata", "documents and settings"];
        if top.ends_with(".sys") || owned.contains(&top.as_str()) {
            return true;
        }
    }
    if names.len() == 2 && top == "users" && ["public", "default"].contains(&names[1].as_str()) {
        return true;
    }
    match top.as_str() {
        "program files"
        | "program files (x86)"
        | "$recycle.bin"
        | "system volume information"
        | "recovery"
        | "boot"
        | "efi" => true,
        "windows" => !(names.len() > 2 && names[1] == "temp"),
        _ => false,
    }
}

/// Virtual disks (Docker Desktop, WSL, Hyper-V): only their own tools may
/// touch them, so neither a disk nor a folder known to hold one may go to
/// the Recycle Bin.
#[cfg(windows)]
fn holds_vm_disk(p: &Path, home: &Path) -> bool {
    let ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    if matches!(ext.as_deref(), Some("vhdx" | "vhd" | "avhdx")) {
        return true;
    }
    let local = standardize(crate::sys::local_app_data());
    [
        local.join("Docker"),
        local.join("wsl"),
        home.join(".docker"),
    ]
    .iter()
    .any(|vm| crate::sys::starts_with(vm, p))
}

#[cfg(windows)]
fn is_vm_home(_p: &Path, _home: &Path) -> bool {
    false // covered by `holds_vm_disk`
}

/// User folders that are never deleted as a whole (their contents may be).
#[cfg(windows)]
fn user_folders(home: &Path) -> Vec<PathBuf> {
    let mut folders: Vec<PathBuf> = [
        "Documents",
        "Desktop",
        "Downloads",
        "Pictures",
        "Videos",
        "Music",
        "OneDrive",
        "AppData",
        r"AppData\LocalLow",
    ]
    .iter()
    .map(|rel| home.join(rel))
    .collect();
    let local = standardize(crate::sys::local_app_data());
    let roaming = standardize(crate::sys::roaming_app_data());
    for rel in ["", "Temp", "Packages", "Microsoft", "Programs"] {
        folders.push(local.join(rel));
    }
    for rel in ["", "Microsoft"] {
        folders.push(roaming.join(rel));
    }
    folders.iter().map(standardize).collect()
}

/// Nera Cleaner's own settings and cache folders.
#[cfg(windows)]
fn is_own_state(p: &Path, _home: &Path) -> bool {
    is_under(p, crate::sys::app_support_dir())
        || crate::sys::app_cache_dir()
            .parent()
            .is_some_and(|dir| is_under(p, dir))
}

// MARK: - Live application state

/// Caches of software that is (or may be) running and breaks when they vanish.
pub fn is_live_app_state(path: &Path) -> bool {
    let p = standardize(path);
    if is_own_state(&p, &standardize(home())) {
        return true;
    }
    for family in ide_families() {
        if same(&p, &family.caches_root) || same(&p, &family.logs_root) {
            return true; // never remove every IDE (and Toolbox) at once
        }
    }
    let Some(found) = ide_system_dir_containing(&p) else {
        return false;
    };
    if !found.family.owns_name(&found.name) {
        // JetBrains/Toolbox, JetBrains/Daemon: live sockets and downloads.
        // Under Caches/Google only AndroidStudio* is IDE state (Chrome is a cache).
        return found.family.name_prefixes.is_empty();
    }
    is_ide_running(&found.system_dir)
}

// MARK: - IDE system directories

/// One JetBrains-platform IDE family and where it keeps per-version state.
#[derive(Clone, Debug)]
pub struct IdeFamily {
    pub caches_root: PathBuf,
    pub logs_root: PathBuf,
    /// Child folder prefixes that belong to an IDE (empty = any child).
    pub name_prefixes: Vec<&'static str>,
    /// Children that are not IDE system dirs and must never be touched.
    pub excluded_names: Vec<&'static str>,
}

impl IdeFamily {
    pub fn owns_name(&self, name: &str) -> bool {
        if name.starts_with('.') || self.excluded_names.contains(&name) {
            return false;
        }
        self.name_prefixes.is_empty() || self.name_prefixes.iter().any(|p| name.starts_with(p))
    }
}

/// Folder holding one "system" dir per IDE and the folder holding its logs.
/// macOS keeps them apart (`~/Library/Caches` and `~/Library/Logs`); on
/// Windows both are `%LOCALAPPDATA%/<vendor>`, with logs in `<IDE>/log`.
fn vendor_roots(vendor: &str) -> (PathBuf, PathBuf) {
    #[cfg(target_os = "macos")]
    {
        let h = standardize(home());
        (
            h.join("Library/Caches").join(vendor),
            h.join("Library/Logs").join(vendor),
        )
    }
    #[cfg(windows)]
    {
        let root = standardize(crate::sys::local_app_data()).join(vendor);
        (root.clone(), root)
    }
}

pub fn jetbrains() -> IdeFamily {
    let (caches_root, logs_root) = vendor_roots("JetBrains");
    IdeFamily {
        caches_root,
        logs_root,
        name_prefixes: vec![],
        excluded_names: vec!["Toolbox", "Daemon"],
    }
}

pub fn android_studio() -> IdeFamily {
    let (caches_root, logs_root) = vendor_roots("Google");
    IdeFamily {
        caches_root,
        logs_root,
        name_prefixes: vec!["AndroidStudio"],
        excluded_names: vec![],
    }
}

pub fn ide_families() -> [IdeFamily; 2] {
    [jetbrains(), android_studio()]
}

/// What "Invalidate Caches" removes; everything else in a system dir is kept.
#[cfg(target_os = "macos")]
pub const IDE_REGENERABLE_SUBFOLDERS: [&str; 3] = ["caches", "index", "tmp"];
/// On Windows the IDE's logs live inside its system dir, in `log`.
#[cfg(windows)]
pub const IDE_REGENERABLE_SUBFOLDERS: [&str; 4] = ["caches", "index", "tmp", "log"];

pub struct IdeSystemDir {
    pub family: IdeFamily,
    pub name: String,
    pub system_dir: PathBuf,
}

/// The IDE system dir that `path` lives in (via its caches OR logs folder).
pub fn ide_system_dir_containing(path: &Path) -> Option<IdeSystemDir> {
    let p = standardize(path);
    for family in ide_families() {
        for root in [family.caches_root.clone(), family.logs_root.clone()] {
            if let Some(rest) = crate::sys::strip_prefix(&p, &root) {
                let name = rest
                    .components()
                    .next()?
                    .as_os_str()
                    .to_string_lossy()
                    .into_owned();
                let system_dir = family.caches_root.join(&name);
                return Some(IdeSystemDir {
                    family,
                    name,
                    system_dir,
                });
            }
        }
    }
    None
}

/// Cleanable targets of a family: `<system>/{caches,index,tmp}` and the
/// IDE's log folder, for IDEs that are not running.
pub fn ide_cleanable_paths(family: &IdeFamily) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let idle =
        |name: &str| family.owns_name(name) && !is_ide_running(&family.caches_root.join(name));
    for name in child_dir_names(&family.caches_root) {
        if !idle(&name) {
            continue;
        }
        for sub in IDE_REGENERABLE_SUBFOLDERS {
            let p = family.caches_root.join(&name).join(sub);
            if p.exists() {
                out.push(p);
            }
        }
    }
    if same(&family.logs_root, &family.caches_root) {
        return out; // logs are a subfolder of the system dir, handled above
    }
    for name in child_dir_names(&family.logs_root) {
        if idle(&name) {
            out.push(family.logs_root.join(name));
        }
    }
    out
}

/// Name of the running IDE whose system/log dir contains `path`, if any.
pub fn running_ide_owning(path: &Path) -> Option<String> {
    let found = ide_system_dir_containing(path)?;
    (found.family.owns_name(&found.name) && is_ide_running(&found.system_dir)).then_some(found.name)
}

/// True when the IDE that owns `system_dir` is running. IDEs write `.pid` and
/// `.home` (their install path) on start but do not always remove
/// `.pid` on exit, so the PID is verified against the install path, with a
/// scan of all processes as fallback when `.pid` is missing or stale.
pub fn is_ide_running(system_dir: &Path) -> bool {
    let install_home = std::fs::read_to_string(system_dir.join(".home"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if let Some(pid) = std::fs::read_to_string(system_dir.join(".pid"))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .filter(|pid| *pid > 0)
    {
        if crate::fs_util::process_alive(pid) {
            match (&install_home, crate::fs_util::executable_path(pid)) {
                (Some(home), Some(exe)) => {
                    if is_under(&exe, home) {
                        return true;
                    }
                }
                // Alive but unverifiable: err on the side of not deleting.
                _ => return true,
            }
        }
    }
    match install_home {
        Some(home) => crate::fs_util::any_process_under(Path::new(&home)),
        None => false,
    }
}

fn child_dir_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn system_roots_are_protected() {
        for p in [
            "/",
            "/System/Library",
            "/usr/bin",
            "/Applications",
            "/private/var/vm/swapfile0",
        ] {
            assert!(!is_trashable(Path::new(p)), "{p}");
        }
        assert!(!is_deletable(&home().join("Library/Caches")));
        assert!(!is_deletable(&home()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn symlinked_ancestors_and_relative_paths_are_refused() {
        // A link to the home folder's parent: the child spelled through the
        // link is the home folder itself.
        let dir = std::env::temp_dir().join(format!("nera-safety-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("users");
        let h = home();
        std::os::unix::fs::symlink(h.parent().unwrap(), &link).unwrap();
        let via_link = link.join(h.file_name().unwrap());
        assert!(!is_trashable(&via_link));
        assert!(!is_deletable(&via_link.join("Documents")));
        // An ordinary file next to the link is still removable.
        assert!(is_deletable(&dir.join("scratch.txt")));
        std::fs::remove_file(&link).unwrap();
        std::fs::remove_dir(&dir).unwrap();

        assert!(!is_trashable(Path::new("Desktop/file.txt")));
        assert!(!is_deletable(Path::new("file.txt")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cloud_drive_state_cannot_be_trashed() {
        let h = home();
        for rel in [
            "Library/Application Support/Google/DriveFS",
            "Library/Application Support/Google/DriveFS/1234/content_cache",
            "Library/Application Support/Google",
            "Library/CloudStorage",
            "Library/CloudStorage/GoogleDrive-me@example.com",
        ] {
            assert!(!is_trashable(&h.join(rel)), "{rel}");
        }
        // A file inside the drive is the user's to remove.
        assert!(is_trashable(&h.join(
            "Library/CloudStorage/GoogleDrive-me@example.com/My Drive/old.zip"
        )));
        assert!(is_trashable(&h.join("Library/Caches/com.google.drivefs")));
        assert!(is_trashable(&h.join(
            "Library/Application Support/Google/Chrome/Default/Cache"
        )));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn container_vm_disks_cannot_be_trashed() {
        let h = home();
        for rel in [
            "Library/Containers/com.docker.docker",
            "Library/Containers/com.docker.docker/Data/vms/0/data/Docker.raw",
            ".docker",
            ".orbstack",
            "Library/Containers",
        ] {
            assert!(!is_trashable(&h.join(rel)), "{rel}");
        }
        assert!(is_trashable(&h.join(
            "Library/Containers/com.docker.docker/Data/log/host/x.log"
        )));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn jetbrains_toolbox_and_roots_are_protected() {
        let h = home();
        for rel in [
            "Library/Caches/JetBrains",
            "Library/Caches/JetBrains/Toolbox",
            "Library/Caches/JetBrains/Toolbox/download",
            "Library/Caches/JetBrains/Daemon",
            "Library/Logs/JetBrains",
            "Library/Caches/Google",
            "Library/Caches/com.nextera.neracleaner/x",
        ] {
            assert!(!is_deletable(&h.join(rel)), "{rel}");
        }
        assert!(is_deletable(&h.join("Library/Caches/Google/Chrome")));
    }

    #[test]
    fn live_pid_with_matching_install_is_running() {
        let dir = std::env::temp_dir().join(format!("nera-ide-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = std::env::current_exe().unwrap();
        std::fs::write(dir.join(".pid"), std::process::id().to_string()).unwrap();
        std::fs::write(
            dir.join(".home"),
            exe.parent().unwrap().to_string_lossy().as_bytes(),
        )
        .unwrap();
        assert!(is_ide_running(&dir));

        // Same live PID, but it is not the IDE (PID reused by another process).
        std::fs::write(dir.join(".home"), "/Applications/NotInstalled.app/Contents").unwrap();
        assert!(!is_ide_running(&dir));

        // Stale PID.
        std::fs::write(dir.join(".pid"), "99999999").unwrap();
        assert!(!is_ide_running(&dir));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(windows)]
    #[test]
    fn system_roots_are_protected() {
        let drive = crate::paths::system_drive();
        for rel in [
            "",
            "Windows",
            "windows/System32/drivers",
            "Windows/Temp",
            "Program Files/Git",
            "Program Files (x86)",
            "ProgramData",
            "Users",
            "Users/Public",
            "pagefile.sys",
            "$Recycle.Bin/S-1-5-21",
        ] {
            assert!(!is_trashable(&drive.join(rel)), "{rel}");
        }
        assert!(is_trashable(&drive.join("Windows/Temp/x.tmp")));
        assert!(is_trashable(&drive.join("ProgramData/Vendor/cache")));
        assert!(!is_trashable(&home()));
        assert!(!is_trashable(Path::new("D:/")));
    }

    #[cfg(windows)]
    #[test]
    fn user_folders_are_kept_but_their_contents_are_not() {
        let h = home();
        let local = crate::sys::local_app_data();
        for path in [
            h.join("Documents"),
            h.join("downloads"),
            h.join("AppData"),
            local.clone(),
            local.join("Temp"),
            local.join("Packages"),
            crate::sys::roaming_app_data(),
        ] {
            assert!(!is_deletable(&path), "{}", path.display());
        }
        assert!(is_deletable(&local.join("Temp/x.tmp")));
        assert!(is_deletable(&h.join("Downloads/setup.exe")));
    }

    #[cfg(windows)]
    #[test]
    fn virtual_disks_cannot_be_trashed() {
        let h = home();
        let local = crate::sys::local_app_data();
        for path in [
            local.join("Docker"),
            local.join("Docker/wsl/disk/docker_data.vhdx"),
            local.join("Packages/Distro/LocalState/ext4.VHDX"),
            h.join(".docker"),
        ] {
            assert!(!is_trashable(&path), "{}", path.display());
        }
        assert!(is_trashable(&local.join("Docker/log/host/x.log")));
    }

    #[cfg(windows)]
    #[test]
    fn jetbrains_toolbox_and_roots_are_protected() {
        let local = crate::sys::local_app_data();
        for rel in [
            "JetBrains",
            "JetBrains/Toolbox",
            "JetBrains/Toolbox/download",
            "Google",
            "Nera Cleaner/Cache/x",
        ] {
            assert!(!is_deletable(&local.join(rel)), "{rel}");
        }
        assert!(!is_deletable(
            &crate::paths::app_support_dir().join("settings.json")
        ));
        assert!(is_deletable(
            &local.join("Google/Chrome/User Data/Default/Cache")
        ));
    }
}
