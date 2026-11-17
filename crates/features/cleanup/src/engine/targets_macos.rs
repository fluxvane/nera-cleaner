//! macOS junk locations (port of `ScanTargets.swift`).

use super::{child_dirs, Category, Group, Mode};
use nera_core::paths::{home, home_join};
use nera_core::safety;
use std::path::{Path, PathBuf};

fn h(rel: &str) -> PathBuf {
    home_join(rel)
}

fn hs(rels: &[&str]) -> Vec<PathBuf> {
    rels.iter().map(|r| h(r)).collect()
}

fn abs(paths: &[&str]) -> Vec<PathBuf> {
    paths.iter().map(PathBuf::from).collect()
}

/// All categories, including the dynamic ones (other users, system temp,
/// app support caches). Reads the file system.
pub fn all_categories() -> Vec<Category> {
    use Group::*;
    let mut out = vec![
        // Caches
        Category::new("user-cache", "User caches", "folder", Caches, hs(&["Library/Caches"]),
            "Application caches. Apps recreate them as needed"),
        Category::new("system-cache", "System caches", "monitor-cog", Caches, abs(&["/Library/Caches"]),
            "System-level caches shared by all accounts. Only items you can remove without an administrator are counted"),
        Category::new("temp-files", "Temporary files", "clock", Caches, hs(&["Library/Caches/TemporaryItems"]),
            "Temporary items left behind by apps"),
        Category::new("http-storages", "HTTP storages", "network", Caches, hs(&["Library/HTTPStorages"]),
            "Cached web responses and cookies of apps"),
        Category::new("quicklook-cache", "Quick Look cache", "eye", Caches,
            hs(&["Library/Caches/com.apple.QuickLook.thumbnailcache"]),
            "Thumbnail previews. macOS rebuilds them"),
        // Logs
        Category::new("user-logs", "User logs", "file-text", Logs, hs(&["Library/Logs"]),
            "Log files written by your apps"),
        Category::new("system-logs", "System logs", "file-text", Logs, abs(&["/Library/Logs", "/private/var/log"]),
            "Log files written by macOS and system services. Only items you can remove without an administrator are counted"),
        Category::new("diagnostic-reports", "Diagnostic reports", "stethoscope", Logs,
            vec![h("Library/Logs/DiagnosticReports"), PathBuf::from("/Library/Logs/DiagnosticReports")],
            "Crash reports and diagnostics"),
        // Browsers
        Category::new("browser-safari", "Safari cache", "compass", Browsers,
            hs(&["Library/Caches/com.apple.Safari", "Library/Safari/LocalStorage", "Library/Safari/Databases"]),
            "Safari cache and website data"),
        Category::new("browser-chrome", "Chrome cache", "globe", Browsers,
            hs(&[
                "Library/Caches/Google/Chrome",
                "Library/Application Support/Google/Chrome/Default/Cache",
                "Library/Application Support/Google/Chrome/Default/Code Cache",
                "Library/Application Support/Google/Chrome/Default/GPUCache",
                "Library/Application Support/Google/Chrome/Default/Service Worker/CacheStorage",
            ]),
            "Google Chrome cache"),
        Category::new("browser-firefox", "Firefox cache", "flame", Browsers, hs(&["Library/Caches/Firefox"]),
            "Mozilla Firefox cache"),
        Category::new("browser-edge", "Edge cache", "globe", Browsers,
            hs(&["Library/Caches/Microsoft Edge", "Library/Application Support/Microsoft Edge/Default/Cache"]),
            "Microsoft Edge cache"),
        Category::new("browser-arc", "Arc cache", "globe", Browsers,
            hs(&["Library/Caches/company.thebrowser.Browser", "Library/Application Support/Arc/User Data/Default/Cache"]),
            "Arc browser cache"),
        Category::new("browser-brave", "Brave cache", "shield", Browsers,
            hs(&["Library/Caches/BraveSoftware", "Library/Application Support/BraveSoftware/Brave-Browser/Default/Cache"]),
            "Brave browser cache"),
        Category::new("browser-opera", "Opera cache", "globe", Browsers, hs(&["Library/Caches/com.operasoftware.Opera"]),
            "Opera browser cache"),
        Category::new("browser-vivaldi", "Vivaldi cache", "globe", Browsers,
            hs(&["Library/Caches/Vivaldi", "Library/Application Support/Vivaldi/Default/Cache"]),
            "Vivaldi browser cache"),
        // Developer tools
        Category::new("xcode-derived", "Xcode DerivedData", "hammer", Developer,
            hs(&["Library/Developer/Xcode/DerivedData", "Library/Developer/Xcode/UserData/Previews"]),
            "Build products and SwiftUI preview builds. Xcode rebuilds them"),
        Category::new("xcode-archives", "Xcode archives", "archive", Developer, hs(&["Library/Developer/Xcode/Archives"]),
            "Archived app builds, needed to symbolicate old crash reports").review(),
        Category::new("ios-device-support", "iOS device support", "smartphone", Developer,
            hs(&[
                "Library/Developer/Xcode/iOS DeviceSupport",
                "Library/Developer/Xcode/watchOS DeviceSupport",
                "Library/Developer/Xcode/tvOS DeviceSupport",
                "Library/Developer/DeveloperDiskImages",
            ]),
            "Debug symbols for connected devices. Xcode copies them again"),
        Category::new("simulators", "iOS simulators", "tablet", Developer,
            hs(&["Library/Developer/CoreSimulator/Devices", "Library/Developer/CoreSimulator/Caches"]),
            "Simulator devices with their installed apps and data").review(),
        Category::new("xcode-cache", "Xcode caches", "hammer", Developer,
            hs(&[
                "Library/Caches/com.apple.dt.Xcode",
                "Library/Developer/Xcode/DocumentationCache",
                "Library/Developer/Xcode/CodingAssistant",
            ]),
            "Xcode app cache, documentation cache and assistant data"),
        Category::new("xcode-logs-products", "Xcode logs and products", "file-clock", Developer,
            hs(&["Library/Developer/Xcode/iOS Device Logs", "Library/Developer/Xcode/Products"]),
            "Device logs and exported build products"),
        Category::new("swiftpm-cache", "SwiftPM cache", "package", Developer,
            hs(&["Library/Caches/org.swift.swiftpm", ".swiftpm/cache"]),
            "Swift Package Manager download cache"),
        Category::new("tuist-cache", "Tuist cache", "box", Developer, hs(&[".tuist/Cache"]), "Tuist build cache"),
        Category::new("mint-cache", "Mint packages", "leaf", Developer, hs(&[".mint"]),
            "Swift tools installed with Mint").review(),
        Category::new("carthage-cache", "Carthage cache", "package", Developer,
            hs(&["Library/Caches/org.carthage.CarthageKit"]), "Carthage dependency cache"),
        Category::new("npm-cache", "npm cache", "package", Developer, hs(&[".npm/_cacache"]), "Node.js package cache"),
        Category::new("yarn-cache", "Yarn cache", "package", Developer, hs(&[".yarn/cache"]), "Yarn package cache"),
        Category::new("pip-cache", "pip cache", "package", Developer, hs(&["Library/Caches/pip", ".cache/pip"]),
            "Python package cache"),
        Category::new("homebrew-cache", "Homebrew cache", "coffee", Developer, hs(&["Library/Caches/Homebrew"]),
            "Homebrew download cache"),
        Category::new("cocoapods-cache", "CocoaPods cache", "package", Developer,
            hs(&[".cocoapods/repos", "Library/Caches/CocoaPods"]), "CocoaPods spec repos and download cache"),
        Category::new("gradle-cache", "Gradle cache", "boxes", Developer, hs(&[".gradle/caches"]),
            "Android and Java build cache"),
        Category::new("maven-cache", "Maven repository", "boxes", Developer, hs(&[".m2/repository"]),
            "Downloaded Maven artifacts, including projects you installed locally with mvn install").review(),
        Category::new("bun-cache", "Bun cache", "package", Developer, hs(&[".bun/install/cache"]), "Bun package cache"),
        Category::new("pnpm-cache", "pnpm cache and store", "package", Developer,
            hs(&["Library/Caches/pnpm", "Library/pnpm/store"]),
            "pnpm cache and content-addressable store").review(),
        Category::new("cargo-cache", "Cargo cache", "package", Developer, hs(&[".cargo/registry/cache"]),
            "Rust crate download cache"),
        Category::new("go-cache", "Go build and module cache", "package", Developer,
            hs(&["Library/Caches/go-build", "go/pkg/mod/cache"]), "Go build cache and module downloads"),
        Category::new("bundler-cache", "Bundler cache", "gem", Developer, hs(&[".bundle/cache"]), "Ruby gem cache"),
        Category::new("composer-cache", "Composer cache", "package", Developer, hs(&[".composer/cache"]),
            "PHP Composer package cache"),
        Category::new("nuget-packages", "NuGet packages", "boxes", Developer, hs(&[".nuget/packages"]),
            ".NET package store. Projects must restore packages again").review(),
        Category::new("conda-packages", "Conda packages", "boxes", Developer,
            hs(&["anaconda3/pkgs", "miniconda3/pkgs", "miniforge3/pkgs"]),
            "Conda package store. Prefer conda clean").review(),
        Category::new("vscode-cache", "VS Code cache", "code-xml", Developer,
            hs(&[
                "Library/Application Support/Code/Cache",
                "Library/Application Support/Code/CachedData",
                "Library/Application Support/Code/CachedExtensions",
                "Library/Application Support/Code/CachedExtensionVSIXs",
                "Library/Application Support/Code/Code Cache",
                "Library/Application Support/Code/GPUCache",
                "Library/Application Support/Code/logs",
                "Library/Caches/com.microsoft.VSCode",
            ]),
            "Visual Studio Code caches and logs"),
        Category::new("jetbrains-cache", "JetBrains caches", "code-xml", Developer,
            safety::ide_cleanable_paths(&safety::jetbrains()),
            "Caches, indexes, temp files and logs of JetBrains IDEs that are not running. Toolbox and running IDEs are never touched"),
        Category::new("android-studio-cache", "Android Studio caches", "smartphone", Developer,
            safety::ide_cleanable_paths(&safety::android_studio()),
            "Caches, indexes, temp files and logs of Android Studio versions that are not running"),
        Category::new("cursor-cache", "Cursor cache", "code-xml", Developer,
            hs(&[
                "Library/Application Support/Cursor/Cache",
                "Library/Application Support/Cursor/CachedData",
                "Library/Application Support/Cursor/Code Cache",
                "Library/Application Support/Cursor/CachedExtensionVSIXs",
                "Library/Application Support/Cursor/logs",
                "Library/Caches/com.todesktop.230313mzl4w4u92",
            ]),
            "Cursor editor caches, extension installers and logs"),
        Category::new("editor-local-history", "Editor local history", "rotate-ccw-clock", Developer,
            hs(&[
                "Library/Application Support/Cursor/User/History",
                "Library/Application Support/Code/User/History",
                "Library/Application Support/Cursor/User/globalStorage/state.vscdb.backup",
                "Library/Application Support/Code/User/globalStorage/state.vscdb.backup",
            ]),
            "VS Code and Cursor file history. Removing it loses local history").review(),
        Category::new("system-simulator-caches", "Simulator system caches", "tablet", Developer,
            abs(&["/Library/Developer/CoreSimulator/Caches", "/Library/Developer/CoreSimulator/Images"]),
            "Shared simulator image caches. Xcode downloads them on demand. Needs an administrator to remove"),
        // Apps
        Category::new("obsidian-cache", "Obsidian cache", "notebook-pen", Apps,
            hs(&["Library/Application Support/obsidian/Cache"]), "Obsidian app cache"),
        Category::new("spotify-cache", "Spotify cache", "music", Apps,
            hs(&["Library/Caches/com.spotify.client", "Library/Application Support/Spotify/PersistentCache"]),
            "Spotify streaming cache"),
        Category::new("slack-cache", "Slack cache", "message-square", Apps,
            hs(&[
                "Library/Application Support/Slack/Cache",
                "Library/Application Support/Slack/Code Cache",
                "Library/Application Support/Slack/Service Worker",
                "Library/Caches/com.tinyspeck.slackmacgap",
            ]),
            "Slack app cache"),
        Category::new("discord-cache", "Discord cache", "message-circle", Apps,
            hs(&[
                "Library/Application Support/discord/Cache",
                "Library/Application Support/discord/Code Cache",
                "Library/Caches/com.hnc.Discord",
            ]),
            "Discord app cache"),
        Category::new("adobe-cache", "Adobe cache", "palette", Apps,
            hs(&[
                "Library/Caches/Adobe",
                "Library/Application Support/Adobe/Common/Media Cache Files",
                "Library/Application Support/Adobe/Common/Media Cache",
                "Library/Application Support/Adobe/Common/Peak Files",
            ]),
            "Premiere Pro and After Effects media cache, and Adobe app caches. The apps rebuild them"),
        Category::new("teams-cache", "Microsoft Teams cache", "users", Apps,
            hs(&["Library/Application Support/Microsoft/Teams/Cache", "Library/Caches/com.microsoft.teams"]),
            "Teams app cache"),
        Category::new("zoom-cache", "Zoom cache", "video", Apps,
            hs(&["Library/Application Support/zoom.us/data", "Library/Caches/us.zoom.xos"]),
            "Zoom cache and local data. Zoom asks you to sign in again")
            // The data folder holds the sign-in and local chat history.
            .review(),
        Category::new("notion-cache", "Notion cache", "notebook-pen", Apps,
            hs(&["Library/Application Support/Notion/Cache", "Library/Caches/notion.id"]), "Notion app cache"),
        Category::new("figma-cache", "Figma cache", "palette", Apps, hs(&["Library/Application Support/Figma/Cache"]),
            "Figma desktop cache"),
        Category::new("postman-cache", "Postman cache", "send", Apps,
            hs(&["Library/Application Support/Postman/Cache", "Library/Caches/com.postmanlabs.mac"]),
            "Postman app cache"),
        Category::new("mail-downloads", "Mail downloads", "mail", Apps,
            hs(&["Library/Mail Downloads", "Library/Containers/com.apple.mail/Data/Library/Mail Downloads"]),
            "Attachments you opened from Mail"),
        // System
        Category::new("trash", "Trash", "trash", System, hs(&[".Trash"]), "Files in your Trash, emptied permanently. Space shared with files elsewhere is not counted")
            // Emptying the Trash cannot be undone: never pre-selected.
            .review(),
        Category::new("saved-app-state", "Saved application state", "layers", System,
            hs(&["Library/Saved Application State"]), "Window restoration state. Apps recreate it"),
        Category::new("old-installers", "Old installers", "download", System, hs(&["Downloads"]),
            "Disk images and packages in Downloads older than 30 days, moved to the Trash")
            .review(),
        Category::new("ios-backups", "iOS backups", "smartphone", System,
            hs(&["Library/Application Support/MobileSync/Backup"]), "Local iPhone and iPad backups").review(),
        Category::new("system-temp-data", "System temporary data", "hourglass", System, system_temp_paths(),
            "Per-user temporary folders and /tmp. Quit apps before cleaning").review(),
        Category::new("macos-update-files", "macOS update files", "cloud-download", System,
            abs(&["/Library/Updates"]), "Downloaded macOS updates that were already applied. Needs an administrator to remove").review(),
        Category::new("core-dumps", "Core dumps", "triangle-alert", System, abs(&["/cores"]),
            "Crash memory dumps, only useful for debugging. Needs an administrator to remove"),
        // AI and virtual machines
        Category::new("docker-desktop-data", "Docker Desktop data", "container", AiVm,
            hs(&["Library/Containers/com.docker.docker", "Library/Group Containers/group.com.docker"]),
            "Docker VM disk, images and containers. Quit Docker first")
            .review()
            .trash_targets(),
        Category::new("orbstack-data", "OrbStack, Lima and Colima data", "container", AiVm,
            hs(&[".orbstack", ".colima", ".lima"]), "Container VM data of OrbStack, Colima and Lima")
            .review()
            .trash_targets(),
        Category::new("lmstudio-models", "LM Studio models", "brain", AiVm, hs(&[".lmstudio", ".cache/lm-studio"]),
            "Downloaded local models. You can download them again")
            .review()
            .trash_targets(),
        Category::new("ollama-models", "Ollama models", "brain", AiVm, hs(&[".ollama"]),
            "Downloaded Ollama models. You can pull them again")
            .review()
            .trash_targets(),
        Category::new("huggingface-cache", "ML model caches", "wand-sparkles", AiVm,
            hs(&[".cache/huggingface", ".cache/torch", ".cache/whisper"]),
            "Hugging Face, PyTorch and Whisper model caches. Apps download them again"),
        Category::new("ai-agent-data", "AI agent data", "bot", AiVm,
            hs(&[
                ".claude",
                ".codex",
                "Library/Application Support/Claude/vm_bundles",
                "Library/Application Support/Claude/claude-code",
                "Library/Application Support/Claude/claude-code-vm",
                "Library/Application Support/Claude/claude-code-sessions",
                "Library/Application Support/Claude/local-agent-mode-sessions",
                "Library/Application Support/Cursor/User/globalStorage/anysphere.cursor-agent-worker",
            ]),
            "The whole .claude and .codex folders (settings, history, sessions), Claude app VM bundles and sessions, and the Cursor agent worker")
            .review()
            .trash_targets(),
        Category::new("virtual-machines", "Virtual machines", "laptop", AiVm,
            hs(&["VirtualBox VMs", "Parallels", "Documents/Virtual Machines.localized", "UTM", "Documents/UTM"]),
            "VirtualBox, Parallels and UTM disk images")
            .review()
            .trash_targets(),
    ];
    if let Some(installers) = out.iter_mut().find(|c| c.id == "old-installers") {
        installers.mode = Mode::OldInstallers;
    }
    out.extend(other_user_categories(Path::new("/Users"), &home()));
    if let Some(c) = app_support_cache_category(&h("Library/Application Support")) {
        out.push(c);
    }
    out
}

/// Caches, logs and Trash of the other accounts on this Mac.
pub fn other_user_categories(users_root: &Path, current_home: &Path) -> Vec<Category> {
    let current = nera_core::paths::standardize(current_home);
    let homes: Vec<PathBuf> = child_dirs(users_root)
        .into_iter()
        .filter(|p| {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            name != "Shared"
                && !name.starts_with('.')
                && nera_core::paths::standardize(p) != current
        })
        .collect();
    if homes.is_empty() {
        return Vec::new();
    }
    let join = |rel: &str| homes.iter().map(|h| h.join(rel)).collect::<Vec<_>>();
    vec![
        Category::new(
            "other-users-caches",
            "Other users' caches",
            "users",
            Group::System,
            join("Library/Caches"),
            "App caches of other accounts on this Mac",
        )
        .review(),
        Category::new(
            "other-users-logs",
            "Other users' logs",
            "users",
            Group::System,
            join("Library/Logs"),
            "Log files of other accounts on this Mac",
        )
        .review(),
        Category::new(
            "other-users-trash",
            "Other users' Trash",
            "trash",
            Group::System,
            join(".Trash"),
            "Trash of other accounts on this Mac, emptied permanently",
        )
        .review(),
    ]
}

/// `/tmp` plus the T, X, C and "Cleanup At Startup" folders of this user's
/// per-user dir in `/var/folders`. The other buckets belong to system
/// accounts (mode 0700, unreadable even with Full Disk Access), so they are
/// left out. The root of `/var/folders` is root-owned.
fn system_temp_paths() -> Vec<PathBuf> {
    system_temp_paths_in(Path::new("/private/var/folders"), unsafe {
        libc::geteuid()
    })
}

fn system_temp_paths_in(folders: &Path, uid: u32) -> Vec<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let mut paths = vec![PathBuf::from("/private/tmp")];
    for bucket in child_dirs(folders) {
        for user_dir in child_dirs(&bucket) {
            for sub in ["T", "X", "C", "Cleanup At Startup"] {
                let p = user_dir.join(sub);
                if std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir() && m.uid() == uid) {
                    paths.push(p);
                }
            }
        }
    }
    paths
}

/// App folders already covered by a dedicated category.
const APP_SUPPORT_COVERED: [&str; 19] = [
    "Code",
    "Cursor",
    "Slack",
    "discord",
    "obsidian",
    "Spotify",
    "Adobe",
    "Microsoft",
    "zoom.us",
    "Notion",
    "Figma",
    "Postman",
    "Google",
    "Microsoft Edge",
    "Arc",
    "BraveSoftware",
    "Vivaldi",
    "MobileSync",
    "Nera Cleaner",
];

/// Electron/Chromium cache folder names inside an app support folder.
const APP_SUPPORT_CACHE_NAMES: [&str; 8] = [
    "Cache",
    "CachedData",
    "Code Cache",
    "GPUCache",
    "CachedExtensionVSIXs",
    "CachedProfilesData",
    "logs",
    "Crashpad",
];

/// Cache folders of apps without a dedicated category, found inside
/// `support_dir` (`~/Library/Application Support`).
pub fn app_support_cache_category(support_dir: &Path) -> Option<Category> {
    let mut paths = Vec::new();
    for app in child_dirs(support_dir) {
        let name = app
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if APP_SUPPORT_COVERED.contains(&name.as_str()) {
            continue;
        }
        for sub in child_dirs(&app) {
            if sub
                .file_name()
                .is_some_and(|n| APP_SUPPORT_CACHE_NAMES.iter().any(|c| n == *c))
            {
                paths.push(sub);
            }
        }
    }
    (!paths.is_empty()).then(|| {
        Category::new(
            "app-support-caches",
            "App support caches",
            "square-stack",
            Group::Caches,
            paths,
            "Cache and log folders inside app support folders. Apps recreate them",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_unique_and_ide_paths_are_narrow() {
        let cats = all_categories();
        let ids: HashSet<_> = cats.iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), cats.len());
        let jb = safety::jetbrains();
        let asx = safety::android_studio();
        for c in cats
            .iter()
            .filter(|c| c.id == "jetbrains-cache" || c.id == "android-studio-cache")
        {
            for p in &c.paths {
                for root in [
                    &jb.caches_root,
                    &jb.logs_root,
                    &asx.caches_root,
                    &asx.logs_root,
                ] {
                    assert_ne!(&nera_core::paths::standardize(p), root);
                }
                assert!(!p.ends_with("Toolbox") && !p.to_string_lossy().contains("/Toolbox/"));
            }
        }
        assert!(cats
            .iter()
            .find(|c| c.id == "trash")
            .unwrap()
            .empties_trash());
        assert_eq!(
            cats.iter().find(|c| c.id == "old-installers").unwrap().mode,
            Mode::OldInstallers
        );
        // Unsafe categories exist and are never safe by default.
        assert!(
            !cats
                .iter()
                .find(|c| c.id == "docker-desktop-data")
                .unwrap()
                .safe
        );
    }

    #[test]
    fn cloud_drives_and_adobe_documents_are_never_junk() {
        // Files synced by Google Drive / iCloud / Creative Cloud, Drive's
        // upload cache and the user's own folders: no category may clean
        // them, a folder inside them (VM folders in Documents are review
        // only), or a folder that holds them.
        let keep = hs(&[
            "Library/CloudStorage",
            "Library/Mobile Documents",
            "Library/Application Support/Google/DriveFS",
            "Library/Application Support/FileProvider",
            "Library/Application Support/Adobe/CoreSync",
            "Library/Application Support/Adobe/Lightroom CC",
            "Library/Application Support/Adobe/Creative Cloud Libraries",
            "Library/Application Support/Adobe/Common/Team Projects Local Hub",
            "Library/Application Support/OneDrive",
            "Library/Preferences",
            "Creative Cloud Files",
            "My Drive",
            "Pictures",
            "Desktop",
        ]);
        for c in all_categories() {
            // "Old installers" only takes old disk images out of Downloads.
            if c.mode == Mode::OldInstallers {
                continue;
            }
            for p in &c.paths {
                for k in &keep {
                    assert!(
                        !nera_core::paths::is_under(p, k) && !nera_core::paths::is_under(k, p),
                        "{} cleans {} (protected: {})",
                        c.id,
                        p.display(),
                        k.display()
                    );
                }
            }
        }
        let cats = all_categories();
        for id in ["trash", "zoom-cache", "ios-backups", "old-installers"] {
            assert!(!cats.iter().find(|c| c.id == id).unwrap().safe, "{id}");
        }
        // Adobe: regenerable caches only.
        let adobe = cats.iter().find(|c| c.id == "adobe-cache").unwrap();
        assert!(adobe.paths.iter().all(|p| {
            let s = p.to_string_lossy();
            s.contains("/Library/Caches/")
                || s.ends_with("/Common/Media Cache Files")
                || s.ends_with("/Common/Media Cache")
                || s.ends_with("/Common/Peak Files")
        }));
    }

    #[test]
    fn system_temp_only_lists_this_users_folders() {
        use std::os::unix::fs::MetadataExt;
        let root = std::env::temp_dir().join(format!("nera-vf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("ab/user1/T")).unwrap();
        std::fs::create_dir_all(root.join("ab/user1/X")).unwrap();
        let me = std::fs::metadata(&root).unwrap().uid();
        let mine = system_temp_paths_in(&root, me);
        assert_eq!(
            mine,
            vec![
                PathBuf::from("/private/tmp"),
                root.join("ab/user1/T"),
                root.join("ab/user1/X")
            ]
        );
        // A uid that owns nothing here (another account) sees only /tmp.
        assert_eq!(
            system_temp_paths_in(&root, me.wrapping_add(1)),
            vec![PathBuf::from("/private/tmp")]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finds_other_users_and_app_support_caches() {
        let root = std::env::temp_dir().join(format!("nera-targets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in [
            "Users/me",
            "Users/alice",
            "Users/Shared",
            "AS/SomeApp/Cache",
            "AS/SomeApp/Data",
            "AS/Slack/Cache",
        ] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let others = other_user_categories(&root.join("Users"), &root.join("Users/me"));
        assert_eq!(others.len(), 3);
        assert_eq!(
            others[0].paths,
            vec![root.join("Users/alice/Library/Caches")]
        );
        assert!(others[2].empties_trash());

        let c = app_support_cache_category(&root.join("AS")).unwrap();
        assert_eq!(c.paths, vec![root.join("AS/SomeApp/Cache")]);
        std::fs::remove_dir_all(root).unwrap();
    }
}
