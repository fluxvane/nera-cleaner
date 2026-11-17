//! Windows junk locations.
//!
//! Deliberately conservative: only folders whose contents Windows or the
//! owning app recreates. Never Prefetch, WinSxS, Windows Update internals, the
//! registry, anything under Program Files, or browser profiles (the Privacy
//! feature owns those). Explorer's thumbnail and icon caches are left out too:
//! Explorer keeps them open and removing part of the set while it runs is not
//! safe (Disk Cleanup asks Explorer to clear them instead).

use super::{child_dirs, Category, Group};
use nera_core::paths::{home_join, local_app_data, roaming_app_data, system_drive};
use nera_core::safety;
use std::path::{Path, PathBuf};

fn local(rels: &[&str]) -> Vec<PathBuf> {
    let root = local_app_data();
    rels.iter().map(|r| root.join(r)).collect()
}

fn roaming(rels: &[&str]) -> Vec<PathBuf> {
    let root = roaming_app_data();
    rels.iter().map(|r| root.join(r)).collect()
}

fn hs(rels: &[&str]) -> Vec<PathBuf> {
    rels.iter().map(home_join).collect()
}

/// A folder from an environment variable, else `fallback` on the system drive.
fn env_dir(name: &str, fallback: &str) -> PathBuf {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| system_drive().join(fallback))
}

/// `C:\Windows`
fn windows_dir() -> PathBuf {
    env_dir("SystemRoot", "Windows")
}

/// `C:\ProgramData`
fn program_data() -> PathBuf {
    env_dir("ProgramData", "ProgramData")
}

/// `C:\$Recycle.Bin`: stands for the Recycle Bin of every drive (see
/// `targets::is_trash_dir`).
fn recycle_bin() -> PathBuf {
    system_drive().join("$Recycle.Bin")
}

/// Description of a category outside your profile, which only an
/// administrator may clean.
fn admin_note(elevated: &'static str, limited: &'static str) -> &'static str {
    if crate::engine::win::is_elevated() {
        elevated
    } else {
        limited
    }
}

/// All categories, including the dynamic ones (idle IDE folders, app caches).
/// Reads the file system.
pub fn all_categories() -> Vec<Category> {
    use Group::*;
    let wer = |root: PathBuf| -> Vec<PathBuf> {
        ["ReportArchive", "ReportQueue", "Temp"]
            .iter()
            .map(|sub| root.join(sub))
            .collect()
    };
    let mut out = vec![
        // Caches
        Category::new("user-temp", "Temporary files", "clock", Caches, local(&["Temp"]),
            "Files apps left in your temporary folder. Files in use are skipped"),
        Category::new("windows-temp", "Windows temporary files", "monitor-cog", Caches,
            vec![windows_dir().join("Temp")],
            admin_note(
                "Temporary files of Windows, services and installers. Files in use are skipped",
                "Temporary files of Windows, services and installers. Needs an administrator: run Nera as administrator to include them",
            )),
        Category::new("shader-cache", "Shader caches", "cpu", Caches,
            local(&[r"D3DSCache", r"NVIDIA\DXCache", r"NVIDIA\GLCache", r"AMD\DxCache", r"AMD\GLCache"]),
            "DirectX and graphics driver shader caches. Games rebuild them, so their next start can be slower"),
        // Logs
        Category::new("crash-dumps", "Crash dumps", "triangle-alert", Logs, local(&["CrashDumps"]),
            "Memory dumps written when apps crashed, only useful for debugging"),
        Category::new("error-reports", "Error reports", "stethoscope", Logs,
            wer(local_app_data().join(r"Microsoft\Windows\WER")),
            "Windows Error Reporting files of your apps, already sent or queued"),
        Category::new("system-error-reports", "System error reports", "stethoscope", Logs,
            wer(program_data().join(r"Microsoft\Windows\WER")),
            admin_note(
                "Windows Error Reporting files of Windows and services",
                "Windows Error Reporting files of Windows and services. Needs an administrator to remove",
            )),
        // Developer tools
        Category::new("vscode-cache", "VS Code cache", "code-xml", Developer,
            roaming(&[
                r"Code\Cache",
                r"Code\CachedData",
                r"Code\CachedExtensions",
                r"Code\CachedExtensionVSIXs",
                r"Code\Code Cache",
                r"Code\GPUCache",
                r"Code\logs",
            ]),
            "Visual Studio Code caches and logs"),
        Category::new("cursor-cache", "Cursor cache", "code-xml", Developer,
            roaming(&[
                r"Cursor\Cache",
                r"Cursor\CachedData",
                r"Cursor\Code Cache",
                r"Cursor\CachedExtensionVSIXs",
                r"Cursor\GPUCache",
                r"Cursor\logs",
            ]),
            "Cursor editor caches, extension installers and logs"),
        Category::new("editor-local-history", "Editor local history", "rotate-ccw-clock", Developer,
            roaming(&[
                r"Cursor\User\History",
                r"Code\User\History",
                r"Cursor\User\globalStorage\state.vscdb.backup",
                r"Code\User\globalStorage\state.vscdb.backup",
            ]),
            "VS Code and Cursor file history. Removing it loses local history").review(),
        Category::new("jetbrains-cache", "JetBrains caches", "code-xml", Developer,
            safety::ide_cleanable_paths(&safety::jetbrains()),
            "Caches, indexes, temp files and logs of JetBrains IDEs that are not running. Toolbox and running IDEs are never touched"),
        Category::new("android-studio-cache", "Android Studio caches", "smartphone", Developer,
            safety::ide_cleanable_paths(&safety::android_studio()),
            "Caches, indexes, temp files and logs of Android Studio versions that are not running"),
        Category::new("npm-cache", "npm cache", "package", Developer,
            vec![local_app_data().join(r"npm-cache\_cacache"), roaming_app_data().join(r"npm-cache\_cacache")],
            "Node.js package cache"),
        Category::new("yarn-cache", "Yarn cache", "package", Developer, local(&[r"Yarn\Cache"]), "Yarn package cache"),
        Category::new("pip-cache", "pip cache", "package", Developer, local(&[r"pip\Cache"]), "Python package cache"),
        Category::new("bun-cache", "Bun cache", "package", Developer, hs(&[r".bun\install\cache"]), "Bun package cache"),
        Category::new("pnpm-cache", "pnpm cache and store", "package", Developer,
            local(&["pnpm-cache", r"pnpm\store"]),
            "pnpm cache and content-addressable store").review(),
        Category::new("nuget-cache", "NuGet HTTP cache", "boxes", Developer,
            local(&[r"NuGet\v3-cache", r"NuGet\plugins-cache"]),
            "Downloaded NuGet package metadata and installers. NuGet downloads them again"),
        Category::new("nuget-packages", "NuGet packages", "boxes", Developer, hs(&[r".nuget\packages"]),
            ".NET package store. Projects must restore packages again").review(),
        Category::new("gradle-cache", "Gradle cache", "boxes", Developer, hs(&[r".gradle\caches"]),
            "Android and Java build cache"),
        Category::new("maven-cache", "Maven repository", "boxes", Developer, hs(&[r".m2\repository"]),
            "Downloaded Maven artifacts, including projects you installed locally with mvn install").review(),
        Category::new("cargo-cache", "Cargo cache", "package", Developer, hs(&[r".cargo\registry\cache"]),
            "Rust crate download cache"),
        Category::new("go-cache", "Go build and module cache", "package", Developer,
            vec![local_app_data().join("go-build"), home_join(r"go\pkg\mod\cache")],
            "Go build cache and module downloads"),
        Category::new("composer-cache", "Composer cache", "package", Developer, local(&["Composer"]),
            "PHP Composer package cache"),
        Category::new("conda-packages", "Conda packages", "boxes", Developer,
            hs(&[r"anaconda3\pkgs", r"miniconda3\pkgs", r"miniforge3\pkgs"]),
            "Conda package store. Prefer conda clean").review(),
        // Apps
        Category::new("obsidian-cache", "Obsidian cache", "notebook-pen", Apps,
            roaming(&[r"obsidian\Cache", r"obsidian\Code Cache", r"obsidian\GPUCache"]), "Obsidian app cache"),
        Category::new("slack-cache", "Slack cache", "message-square", Apps,
            roaming(&[r"Slack\Cache", r"Slack\Code Cache", r"Slack\GPUCache"]), "Slack app cache"),
        Category::new("discord-cache", "Discord cache", "message-circle", Apps,
            roaming(&[r"discord\Cache", r"discord\Code Cache", r"discord\GPUCache"]), "Discord app cache"),
        Category::new("notion-cache", "Notion cache", "notebook-pen", Apps,
            roaming(&[r"Notion\Cache", r"Notion\Code Cache", r"Notion\GPUCache"]), "Notion app cache"),
        Category::new("postman-cache", "Postman cache", "send", Apps,
            roaming(&[r"Postman\Cache", r"Postman\Code Cache", r"Postman\GPUCache"]), "Postman app cache"),
        Category::new("adobe-cache", "Adobe cache", "palette", Apps,
            roaming(&[r"Adobe\Common\Media Cache Files", r"Adobe\Common\Peak Files"]),
            "Adobe media caches"),
        // System
        Category::new("trash", "Recycle Bin", "trash", System, vec![recycle_bin()],
            "Files in the Recycle Bin of every drive, emptied permanently").review(),
        Category::new("ios-backups", "iOS backups", "smartphone", System,
            vec![roaming_app_data().join(r"Apple Computer\MobileSync\Backup"), home_join(r"Apple\MobileSync\Backup")],
            "Local iPhone and iPad backups").review(),
        // AI and virtual machines
        Category::new("huggingface-cache", "ML model caches", "wand-sparkles", AiVm,
            hs(&[r".cache\huggingface", r".cache\torch", r".cache\whisper"]),
            "Hugging Face, PyTorch and Whisper model caches. Apps download them again"),
    ];
    if let Some(c) = app_cache_category(&roaming_app_data()) {
        out.push(c);
    }
    out
}

/// App folders already covered by a dedicated category, browsers (the Privacy
/// feature) and Nera itself.
const APP_FOLDERS_COVERED: [&str; 14] = [
    "Code",
    "Cursor",
    "Slack",
    "discord",
    "obsidian",
    "Notion",
    "Postman",
    "Adobe",
    "Microsoft",
    "Mozilla",
    "Opera Software",
    "Apple Computer",
    "Nera Cleaner",
    "JetBrains",
];

/// Electron/Chromium cache folder names inside an app folder.
const APP_CACHE_NAMES: [&str; 8] = [
    "Cache",
    "CachedData",
    "Code Cache",
    "GPUCache",
    "CachedExtensionVSIXs",
    "CachedProfilesData",
    "logs",
    "Crashpad",
];

/// Cache folders of desktop apps without a dedicated category, found directly
/// inside each app folder of `roaming` (`%APPDATA%`), where Electron apps keep
/// them. Browser caches sit deeper (`User Data\Default\Cache`) and are never
/// matched.
pub fn app_cache_category(roaming: &Path) -> Option<Category> {
    let mut paths = Vec::new();
    for app in child_dirs(roaming) {
        let name = app
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if APP_FOLDERS_COVERED
            .iter()
            .any(|c| c.eq_ignore_ascii_case(&name))
        {
            continue;
        }
        for sub in child_dirs(&app) {
            if sub
                .file_name()
                .is_some_and(|n| APP_CACHE_NAMES.iter().any(|c| n.eq_ignore_ascii_case(c)))
            {
                paths.push(sub);
            }
        }
    }
    (!paths.is_empty()).then(|| {
        Category::new(
            "app-support-caches",
            "App caches",
            "square-stack",
            Group::Caches,
            paths,
            "Cache, log and crash report folders of desktop apps. Apps recreate them",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::targets::is_trash_dir;
    use std::collections::HashSet;

    #[test]
    fn ids_are_unique_and_locations_are_conservative() {
        let cats = all_categories();
        let ids: HashSet<_> = cats.iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), cats.len());
        let jb = safety::jetbrains();
        let asx = safety::android_studio();
        for c in &cats {
            for p in &c.paths {
                let s = p.to_string_lossy().to_ascii_lowercase();
                for banned in [
                    r"\prefetch",
                    r"\winsxs",
                    r"\softwaredistribution",
                    r"\program files",
                    r"\user data\",
                    r"\explorer\thumbcache",
                ] {
                    assert!(!s.contains(banned), "{} lists {}", c.id, p.display());
                }
                // Every target except the Recycle Bin is emptied item by item
                // through the safety rules: its contents must be removable.
                if !is_trash_dir(p) && c.id != "jetbrains-cache" && c.id != "android-studio-cache" {
                    assert!(
                        nera_core::safety::is_deletable(&p.join("probe")),
                        "{} lists a protected folder {}",
                        c.id,
                        p.display()
                    );
                }
                if c.id == "jetbrains-cache" || c.id == "android-studio-cache" {
                    for root in [&jb.caches_root, &asx.caches_root] {
                        assert_ne!(&nera_core::paths::standardize(p), root);
                    }
                    assert!(!s.contains(r"\toolbox"));
                }
            }
        }
        let trash = cats.iter().find(|c| c.id == "trash").unwrap();
        assert!(trash.empties_trash() && !trash.safe);
        // Temp folders: the folder itself is protected, its contents are not.
        let temp = &cats.iter().find(|c| c.id == "user-temp").unwrap().paths[0];
        assert!(!nera_core::safety::is_deletable(temp));
        assert!(nera_core::safety::is_deletable(&temp.join("junk.tmp")));
        let wtemp = &cats.iter().find(|c| c.id == "windows-temp").unwrap().paths[0];
        assert!(!nera_core::safety::is_deletable(wtemp));
        assert!(nera_core::safety::is_deletable(&wtemp.join("junk.tmp")));
    }

    #[test]
    fn finds_electron_caches_but_not_covered_apps_or_browser_profiles() {
        let root = std::env::temp_dir().join(format!("nera-targets-win-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in [
            r"SomeApp\Cache",
            r"SomeApp\Data",
            r"SomeApp\Code Cache",
            r"slack\Cache",
            r"Opera Software\Opera Stable\Cache",
            r"Other\User Data\Default\Cache",
        ] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let c = app_cache_category(&root).unwrap();
        assert_eq!(
            c.paths,
            vec![
                root.join(r"SomeApp\Cache"),
                root.join(r"SomeApp\Code Cache")
            ]
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
