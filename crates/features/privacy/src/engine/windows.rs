//! Windows side of the privacy engine: where browsers keep their data under
//! `%LOCALAPPDATA%` / `%APPDATA%`, where they install, and when one counts as
//! running.
//!
//! What macOS has and Windows does not, so it is absent here: Safari, the
//! Full Disk Access permission and the Apple menu's recent-items lists.

use super::{Access, Chromium, Engine, Firefox, Group, Spec};
use nera_core::fs_util::{self, Cancel};
use nera_core::paths;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// `Spec::app` of Chromium, which runs as `chrome.exe` like Google Chrome.
const CHROMIUM_APP: &str = "Chromium";

/// Where to look. [`Env::real`] for the app, fixture dirs in tests.
#[derive(Clone, Debug)]
pub struct Env {
    /// `%LOCALAPPDATA%`: Chromium profiles, Firefox caches.
    pub local: PathBuf,
    /// `%APPDATA%`: Firefox and Opera profiles.
    pub roaming: PathBuf,
    /// Folders browsers install into: both Program Files folders, and
    /// `%LOCALAPPDATA%` / `%LOCALAPPDATA%\Programs` for per-user installs.
    pub program_dirs: Vec<PathBuf>,
}

impl Env {
    pub fn real() -> Self {
        let local = paths::local_app_data();
        let mut program_dirs: Vec<PathBuf> = Vec::new();
        let candidates = ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"]
            .iter()
            .filter_map(std::env::var_os)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .chain([local.clone(), local.join("Programs")]);
        for dir in candidates {
            if !program_dirs
                .iter()
                .any(|d| paths::is_under(d, &dir) && paths::is_under(&dir, d))
            {
                program_dirs.push(dir);
            }
        }
        Self {
            roaming: paths::roaming_app_data(),
            local,
            program_dirs,
        }
    }

    /// The browser's executable, when one of its installers put it in a program folder.
    pub(super) fn find_app(&self, app: &str) -> Option<PathBuf> {
        let rels = install_paths(app);
        self.program_dirs
            .iter()
            .flat_map(|dir| rels.iter().map(move |rel| dir.join(rel)))
            .find(|p| p.is_file())
    }
}

/// Where each browser's installer puts its executable, relative to a program
/// folder (system-wide and per-user installs use the same layout).
fn install_paths(app: &str) -> &'static [&'static str] {
    match app {
        "chrome.exe" => &[r"Google\Chrome\Application\chrome.exe"],
        "msedge.exe" => &[r"Microsoft\Edge\Application\msedge.exe"],
        "firefox.exe" => &[r"Mozilla Firefox\firefox.exe"],
        "brave.exe" => &[r"BraveSoftware\Brave-Browser\Application\brave.exe"],
        "opera.exe" => &[r"Opera\opera.exe"],
        "vivaldi.exe" => &[r"Vivaldi\Application\vivaldi.exe"],
        CHROMIUM_APP => &[r"Chromium\Application\chrome.exe"],
        _ => &[],
    }
}

// MARK: - Browser catalogue

pub(super) fn specs(env: &Env) -> Vec<Spec> {
    // Chromium browsers keep `Local State`, the profiles and their caches
    // together in `%LOCALAPPDATA%\<vendor>\User Data`.
    let user_data = |rel: &str| {
        Engine::Chromium(Chromium {
            support: env.local.join(rel),
            cache_root: None,
            flat_caches: Vec::new(),
            cache_mirror: None,
            root_is_profile: false,
        })
    };
    // Opera keeps its profile in `%APPDATA%` and its disk cache in the same
    // layout under `%LOCALAPPDATA%`. Older versions use the folder itself as
    // the only profile, newer ones a `Default` profile inside it.
    let opera = env.roaming.join(r"Opera Software\Opera Stable");
    let opera_root_is_profile = !opera.join("Default").is_dir();
    vec![
        Spec {
            id: "chrome",
            name: "Google Chrome",
            app: "chrome.exe",
            icon: "globe",
            engine: user_data(r"Google\Chrome\User Data"),
        },
        Spec {
            id: "firefox",
            name: "Firefox",
            app: "firefox.exe",
            icon: "flame",
            engine: Engine::Firefox(Firefox {
                support: env.roaming.join(r"Mozilla\Firefox"),
                cache_root: env.local.join(r"Mozilla\Firefox"),
            }),
        },
        Spec {
            id: "edge",
            name: "Microsoft Edge",
            app: "msedge.exe",
            icon: "globe",
            engine: user_data(r"Microsoft\Edge\User Data"),
        },
        Spec {
            id: "brave",
            name: "Brave",
            app: "brave.exe",
            icon: "shield",
            engine: user_data(r"BraveSoftware\Brave-Browser\User Data"),
        },
        Spec {
            id: "opera",
            name: "Opera",
            app: "opera.exe",
            icon: "circle-dot",
            engine: Engine::Chromium(Chromium {
                support: opera,
                cache_root: None,
                flat_caches: Vec::new(),
                cache_mirror: Some(env.local.join(r"Opera Software\Opera Stable")),
                root_is_profile: opera_root_is_profile,
            }),
        },
        Spec {
            id: "vivaldi",
            name: "Vivaldi",
            app: "vivaldi.exe",
            icon: "globe",
            engine: user_data(r"Vivaldi\User Data"),
        },
        Spec {
            id: "chromium",
            name: "Chromium",
            app: CHROMIUM_APP,
            icon: "globe",
            engine: user_data(r"Chromium\User Data"),
        },
    ]
}

/// Windows has no Full Disk Access: every browser folder of the user is readable.
pub(super) fn full_disk_access(_env: &Env) -> Access {
    Access::Unknown
}

/// No non-browser groups on Windows (the Apple menu's recent items are macOS only).
pub(super) fn system_groups(_env: &Env, _cancel: &Cancel, _progress: &dyn Fn(&str)) -> Vec<Group> {
    Vec::new()
}

/// Size of `path` as File Explorer reports it, counting every hard-linked
/// file once (`seen` holds the volume and file index of multiply-linked files
/// already counted). Junctions and symlinks are not followed: std reports
/// them as links, not folders.
pub(super) fn unique_size(path: &Path, seen: &mut HashSet<(u64, u64)>, cancel: &Cancel) -> u64 {
    fn once(path: &Path, meta: &std::fs::Metadata, seen: &mut HashSet<(u64, u64)>) -> u64 {
        if meta.is_file()
            && fs_util::file_key(path, meta).is_some_and(|k| k.links > 1 && !seen.insert(k.id()))
        {
            return 0;
        }
        fs_util::allocated_size_at(path, meta)
    }
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    let mut total = once(path, &meta, seen);
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
            let child = entry.path();
            total += once(&child, &meta, seen);
            if meta.is_dir() {
                stack.push(child);
            }
        }
    }
    total
}

/// True when a running executable is this browser (`app` is its `Spec::app`).
///
/// Every process of a Chromium browser or Firefox runs the browser's own
/// executable, including the ones that stay in the background after the last
/// window closes (Edge startup boost, Chrome background apps); all of them
/// keep the profile files locked. Microsoft Edge WebView2
/// (`msedgewebview2.exe`) is a different program and does not count.
pub fn is_running(app: &str, exes: &[PathBuf]) -> bool {
    exes.iter().any(|exe| runs_as(app, exe))
}

fn runs_as(app: &str, exe: &Path) -> bool {
    let Some(name) = exe.file_name() else {
        return false;
    };
    let in_dir = |dir: &str| {
        exe.components()
            .any(|c| c.as_os_str().eq_ignore_ascii_case(dir))
    };
    match app {
        // Chrome and Chromium share `chrome.exe`. A copy in a folder that names
        // neither counts as both, so neither is cleaned while it runs.
        "chrome.exe" => name.eq_ignore_ascii_case("chrome.exe") && !in_dir("Chromium"),
        CHROMIUM_APP => {
            name.eq_ignore_ascii_case("chrome.exe") && !(in_dir("Google") && in_dir("Chrome"))
        }
        _ => name.eq_ignore_ascii_case(app),
    }
}

// MARK: - Tests

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::{runs_as, CHROMIUM_APP};
    use std::fs;

    struct Fixture {
        root: PathBuf,
        env: Env,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("nera-privacy-win-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            let env = Env {
                local: root.join("Local"),
                roaming: root.join("Roaming"),
                program_dirs: vec![root.join("Program Files"), root.join("Local")],
            };
            for dir in [&env.local, &env.roaming, &env.program_dirs[0]] {
                fs::create_dir_all(dir).unwrap();
            }
            Self { root, env }
        }

        /// Install an executable under the fixture's Program Files.
        fn exe(&self, rel: &str) {
            let path = self.env.program_dirs[0].join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"MZ").unwrap();
        }

        fn local(&self, rel: &str, len: usize) -> PathBuf {
            write(&self.env.local.join(rel), len)
        }

        fn roaming(&self, rel: &str, len: usize) -> PathBuf {
            write(&self.env.roaming.join(rel), len)
        }
    }

    fn write(path: &Path, len: usize) -> PathBuf {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![7u8; len]).unwrap();
        path.to_path_buf()
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

    fn all_paths(g: &Group) -> Vec<String> {
        g.items
            .iter()
            .flat_map(|i| i.targets.iter())
            .map(|t| t.path.to_string_lossy().into_owned())
            .collect()
    }

    const CHROME: &str = r"Google\Chrome\User Data";

    /// Two profiles laid out as current Chrome on Windows does: cookies in
    /// `Network\`, the disk cache in `Cache\Cache_Data`, all under `User Data`.
    fn chrome_fixture(tag: &str) -> Fixture {
        let f = Fixture::new(tag);
        f.exe(r"Google\Chrome\Application\chrome.exe");
        f.local(&format!(r"{CHROME}\Local State"), 0);
        fs::write(
            f.env.local.join(CHROME).join("Local State"),
            r#"{"profile":{"info_cache":{"Default":{"name":"Person 1"},"Profile 2":{"name":"Work"},"Profile 9":{"name":"Gone"}}}}"#,
        )
        .unwrap();
        for p in ["Default", "Profile 2"] {
            f.local(&format!(r"{CHROME}\{p}\Network\Cookies"), 5000);
            f.local(&format!(r"{CHROME}\{p}\Network\Cookies-journal"), 500);
            f.local(&format!(r"{CHROME}\{p}\Network\TransportSecurity"), 700);
            f.local(&format!(r"{CHROME}\{p}\History"), 8000);
            f.local(&format!(r"{CHROME}\{p}\Cache\Cache_Data\data_0"), 12_000);
            f.local(&format!(r"{CHROME}\{p}\Code Cache\js\index"), 1000);
            f.local(
                &format!(r"{CHROME}\{p}\Local Storage\leveldb\000003.log"),
                4000,
            );
            f.local(&format!(r"{CHROME}\{p}\Web Data"), 3000);
            f.local(
                &format!(r"{CHROME}\{p}\Service Worker\CacheStorage\a\b"),
                6000,
            );
            f.local(
                &format!(r"{CHROME}\{p}\Service Worker\Database\000003.log"),
                2000,
            );
            // Never offered.
            f.local(&format!(r"{CHROME}\{p}\Login Data"), 9000);
            f.local(&format!(r"{CHROME}\{p}\Bookmarks"), 1000);
            f.local(&format!(r"{CHROME}\{p}\Extensions\ext\manifest.json"), 1000);
            f.local(
                &format!(r"{CHROME}\{p}\Local Extension Settings\ext\x"),
                1000,
            );
        }
        f.local(&format!(r"{CHROME}\ShaderCache\data_0"), 2500);
        f.local(&format!(r"{CHROME}\Safe Browsing\list"), 2500);
        f
    }

    #[test]
    fn chromium_profiles_come_from_local_state_and_disk() {
        let f = chrome_fixture("profiles");
        f.local(&format!(r"{CHROME}\System Profile\Preferences"), 10);
        f.local(&format!(r"{CHROME}\Profile 4\Preferences"), 10);
        let profiles = chromium_profiles(&f.env.local.join(CHROME));
        let dirs: Vec<&str> = profiles.iter().map(|p| p.dir.as_str()).collect();
        assert_eq!(dirs, ["Default", "Profile 2", "Profile 4"]);
        assert_eq!(profiles[1].name, "Work");
    }

    #[test]
    fn scans_chrome_across_profiles() {
        let f = chrome_fixture("scan-chrome");
        let r = scan_fixture(&f, &[]);
        assert_eq!(r.full_disk_access, Access::Unknown);
        let g = group(&r, "chrome");
        assert_eq!(g.detail, "2 profiles");
        assert!(g.installed && !g.running);
        assert_eq!(g.app_name, Some("chrome.exe"));

        let cookies = item_of(g, DataKind::Cookies);
        assert_eq!(cookies.targets.len(), 4, "Network cookies of two profiles");
        assert_eq!(cookies.bytes, 2 * 5500);
        assert!(cookies
            .targets
            .iter()
            .all(|t| !t.path.ends_with("TransportSecurity")));

        let cache = item_of(g, DataKind::Cache);
        let paths: Vec<_> = cache.targets.iter().map(|t| t.path.clone()).collect();
        assert!(paths.iter().any(|p| p.ends_with(r"Profile 2\Cache")));
        assert!(paths
            .iter()
            .any(|p| p.ends_with(r"Profile 2\Service Worker\CacheStorage")));
        assert!(paths.iter().any(|p| p.ends_with(r"User Data\ShaderCache")));
        assert_eq!(cache.bytes, 2 * (12_000 + 1000 + 6000) + 2500);
        assert_eq!(item_of(g, DataKind::History).bytes, 16_000);
        assert_eq!(item_of(g, DataKind::WebsiteData).bytes, 2 * (4000 + 2000));
        assert_eq!(item_of(g, DataKind::FormData).bytes, 6000);

        for forbidden in [
            "Login Data",
            "Bookmarks",
            "Extensions",
            "Local Extension Settings",
            "Safe Browsing",
        ] {
            assert!(
                all_paths(g).iter().all(|p| !p.contains(forbidden)),
                "{forbidden} must not be offered"
            );
        }
    }

    #[test]
    fn without_browsers_nothing_is_listed() {
        let f = Fixture::new("none");
        let r = scan_fixture(&f, &[]);
        assert!(
            r.groups.is_empty(),
            "no Safari and no recent items on Windows"
        );
        assert_eq!(r.full_disk_access, Access::Unknown);
    }

    #[test]
    fn installed_browser_without_data_is_listed_and_leftover_data_too() {
        let f = Fixture::new("installed");
        f.exe(r"Microsoft\Edge\Application\msedge.exe");
        f.local(
            r"BraveSoftware\Brave-Browser\User Data\Default\Network\Cookies",
            3000,
        );
        let r = scan_fixture(&f, &[]);
        let edge = group(&r, "edge");
        assert!(edge.installed);
        assert_eq!(edge.detail, "No profiles yet");
        let brave = group(&r, "brave");
        assert!(!brave.installed);
        assert_eq!(item_of(brave, DataKind::Cookies).bytes, 3000);
        let ids: Vec<&str> = r.groups.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids, ["edge", "brave"]);
    }

    #[test]
    fn per_user_installs_are_found() {
        let f = Fixture::new("per-user");
        // Fixture's second program folder is its %LOCALAPPDATA%.
        write(&f.env.local.join(r"Vivaldi\Application\vivaldi.exe"), 2);
        f.local(r"Vivaldi\User Data\Default\History", 100);
        let r = scan_fixture(&f, &[]);
        assert!(group(&r, "vivaldi").installed);
    }

    #[test]
    fn firefox_profiles_from_roaming_with_caches_in_local() {
        let f = Fixture::new("firefox");
        f.exe(r"Mozilla Firefox\firefox.exe");
        let ff = r"Mozilla\Firefox";
        for p in ["Profiles/aaa.default", "Profiles/bbb.default-release"] {
            let dir = format!(r"{ff}\{p}");
            f.roaming(&format!(r"{dir}\cookies.sqlite"), 4000);
            f.roaming(&format!(r"{dir}\cookies.sqlite-wal"), 1000);
            f.roaming(&format!(r"{dir}\formhistory.sqlite"), 2000);
            f.roaming(&format!(r"{dir}\places.sqlite"), 9000);
            f.roaming(&format!(r"{dir}\logins.json"), 9000);
            f.roaming(&format!(r"{dir}\key4.db"), 9000);
            f.roaming(
                &format!(r"{dir}\storage\default\https+++example.com\ls\data.sqlite"),
                3000,
            );
            f.roaming(
                &format!(r"{dir}\storage\default\moz-extension+++abc\idb\x"),
                3000,
            );
        }
        f.local(
            &format!(r"{ff}\Profiles\bbb.default-release\cache2\entries\e1"),
            7000,
        );
        f.local(
            &format!(r"{ff}\Profiles\bbb.default-release\startupCache\s"),
            500,
        );
        fs::write(
            f.env.roaming.join(ff).join("profiles.ini"),
            "[Install308046B0AF4A39CB]\nDefault=Profiles/bbb.default-release\nLocked=1\n\n\
             [Profile1]\nName=default\nIsRelative=1\nPath=Profiles/aaa.default\nDefault=1\n\n\
             [Profile0]\nName=default-release\nIsRelative=1\nPath=Profiles/bbb.default-release\n\n\
             [General]\nStartWithLastProfile=1\nVersion=2\n\n\
             [BackgroundTasksProfiles]\nMozillaBackgroundTask-x-defaultagent=1aqe9pug.MozillaBackgroundTask-x\n",
        )
        .unwrap();

        let r = scan_fixture(&f, &[]);
        let g = group(&r, "firefox");
        assert!(g.installed);
        assert_eq!(g.detail, "2 profiles");
        let cache = item_of(g, DataKind::Cache);
        assert_eq!(cache.bytes, 7500);
        assert!(cache
            .targets
            .iter()
            .all(|t| t.path.starts_with(&f.env.local)));
        assert_eq!(item_of(g, DataKind::Cookies).targets.len(), 4);
        assert_eq!(item_of(g, DataKind::Cookies).bytes, 2 * 5000);
        assert!(g.items.iter().all(|i| i.kind != DataKind::History));
        let all = all_paths(g);
        assert!(all.iter().any(|p| p.contains("https+++example.com")));
        for forbidden in ["moz-extension", "places.sqlite", "logins.json", "key4.db"] {
            assert!(all.iter().all(|p| !p.contains(forbidden)), "{forbidden}");
        }
    }

    #[test]
    fn opera_profile_in_roaming_and_cache_in_local() {
        let f = Fixture::new("opera");
        f.exe(r"Opera\opera.exe");
        let root = r"Opera Software\Opera Stable";
        f.roaming(&format!(r"{root}\Network\Cookies"), 3000);
        f.roaming(&format!(r"{root}\History"), 2000);
        f.roaming(&format!(r"{root}\Login Data"), 9000);
        f.local(&format!(r"{root}\Cache\Cache_Data\data_1"), 8000);
        f.local(&format!(r"{root}\unknown.dat"), 9000);
        let r = scan_fixture(&f, &[]);
        let g = group(&r, "opera");
        assert_eq!(g.detail, "1 profile");
        assert_eq!(item_of(g, DataKind::Cookies).bytes, 3000);
        assert_eq!(item_of(g, DataKind::History).bytes, 2000);
        let cache = item_of(g, DataKind::Cache);
        assert_eq!(
            cache.bytes, 8000,
            "only known cache folders, never the whole local folder"
        );
        assert!(all_paths(g).iter().all(|p| !p.contains("Login Data")));

        // Newer layout: a `Default` profile inside the folder.
        let f = Fixture::new("opera-default");
        f.exe(r"Opera\opera.exe");
        f.roaming(&format!(r"{root}\Default\Network\Cookies"), 3000);
        f.local(&format!(r"{root}\Default\Cache\Cache_Data\data_1"), 8000);
        let r = scan_fixture(&f, &[]);
        let g = group(&r, "opera");
        assert_eq!(item_of(g, DataKind::Cookies).bytes, 3000);
        assert_eq!(item_of(g, DataKind::Cache).bytes, 8000);
    }

    #[test]
    fn running_detection_uses_the_executable_name() {
        let chrome = PathBuf::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe");
        let chromium = PathBuf::from(r"C:\Users\me\AppData\Local\Chromium\Application\chrome.exe");
        let portable = PathBuf::from(r"D:\Tools\chrome-win\CHROME.EXE");
        let webview = PathBuf::from(
            r"C:\Program Files (x86)\Microsoft\EdgeWebView\Application\131.0\msedgewebview2.exe",
        );
        let edge = PathBuf::from(r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe");

        assert!(runs_as("chrome.exe", &chrome) && !runs_as(CHROMIUM_APP, &chrome));
        assert!(runs_as(CHROMIUM_APP, &chromium) && !runs_as("chrome.exe", &chromium));
        assert!(runs_as("chrome.exe", &portable) && runs_as(CHROMIUM_APP, &portable));
        assert!(!is_running("msedge.exe", std::slice::from_ref(&webview)));
        assert!(is_running("msedge.exe", &[webview, edge]));
        assert!(!is_running("firefox.exe", std::slice::from_ref(&chrome)));
        assert!(is_running(
            "firefox.exe",
            &[PathBuf::from(
                r"C:\Program Files\Mozilla Firefox\firefox.exe"
            )]
        ));

        let f = chrome_fixture("running");
        let r = scan_fixture(&f, &[chrome]);
        assert!(group(&r, "chrome").running);
    }

    #[test]
    fn clean_removes_selected_data_and_keeps_everything_else() {
        let f = chrome_fixture("clean");
        let r = scan_fixture(&f, &[]);
        let g = group(&r, "chrome");
        let selected: Vec<CleanItem> = g.items.iter().map(|i| CleanItem::from_item(g, i)).collect();
        let expect_freed = g.bytes();

        let report = clean(&selected, &[], false, &Cancel::new(), &|_| {});
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(report.freed, expect_freed);
        assert_eq!(report.cleaned.len(), g.items.len());

        let profile_root = f.env.local.join(CHROME);
        for p in ["Default", "Profile 2"] {
            for gone in [
                r"Network\Cookies",
                "History",
                "Web Data",
                "Cache",
                "Local Storage",
                r"Service Worker\CacheStorage",
            ] {
                assert!(!profile_root.join(p).join(gone).exists(), "{p}/{gone}");
            }
            for kept in [
                r"Network\TransportSecurity",
                "Login Data",
                "Bookmarks",
                "Extensions",
                "Local Extension Settings",
                "Service Worker",
            ] {
                assert!(profile_root.join(p).join(kept).exists(), "{p}/{kept}");
            }
        }
        assert!(profile_root.join("Local State").exists());
        assert!(profile_root.join("Safe Browsing").exists());
        assert_eq!(group(&scan_fixture(&f, &[]), "chrome").bytes(), 0);
    }

    #[test]
    fn clean_skips_running_browsers_without_touching_them() {
        let f = chrome_fixture("clean-running");
        let r = scan_fixture(&f, &[]);
        let g = group(&r, "chrome");
        let selected: Vec<CleanItem> = g.items.iter().map(|i| CleanItem::from_item(g, i)).collect();
        let exes = vec![PathBuf::from(
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        )];
        let report = clean(&selected, &exes, false, &Cancel::new(), &|_| {});
        assert_eq!(report.skipped_running, ["Google Chrome"]);
        assert_eq!(report.removed, 0);
        assert!(f
            .env
            .local
            .join(CHROME)
            .join(r"Default\Network\Cookies")
            .exists());
    }

    #[test]
    fn clean_reports_refusals_and_stops_when_cancelled() {
        let f = Fixture::new("clean-fail");
        let real_root = nera_core::paths::local_app_data().join("Microsoft");
        // Checked first, so a broken rule fails here instead of deleting real data.
        assert!(!nera_core::safety::is_deletable(&real_root));
        let protected = CleanItem {
            id: "edge:cache".into(),
            group_name: "Microsoft Edge".into(),
            app_name: Some("msedge.exe"),
            kind: DataKind::Cache,
            targets: vec![Target {
                path: real_root,
                bytes: 1,
            }],
        };
        let report = clean(&[protected], &[], false, &Cancel::new(), &|_| {});
        assert_eq!(
            report.failed.len(),
            1,
            "nera_core::safety keeps the Microsoft folder"
        );
        assert!(report.cleaned.is_empty());

        let file = f.local(r"Google\Chrome\User Data\Default\History", 100);
        let cancel = Cancel::new();
        cancel.cancel();
        let item = CleanItem {
            id: "chrome:history".into(),
            group_name: "Google Chrome".into(),
            app_name: Some("chrome.exe"),
            kind: DataKind::History,
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
    fn hard_links_are_counted_once() {
        let f = Fixture::new("hardlinks");
        let a = f.local(
            r"Google\Chrome\User Data\Default\Cache\Cache_Data\a",
            40_000,
        );
        let b = f.local(
            r"Google\Chrome\User Data\Default\Cache\Cache_Data\b",
            20_000,
        );
        fs::hard_link(&a, a.with_file_name("a2")).unwrap();
        let r = scan_fixture(&f, &[]);
        let cache = item_of(group(&r, "chrome"), DataKind::Cache);
        assert_eq!(
            cache.bytes, 60_000,
            "the second link to the same file adds nothing"
        );
        assert!(b.exists());
    }

    /// The folders this engine targets on the real machine are removable by
    /// `nera_core::safety` while the vendor roots stay protected. Nothing is touched.
    #[test]
    fn real_browser_locations_pass_safety() {
        use nera_core::safety::{is_deletable, is_trashable};
        let env = Env::real();
        for rel in [
            r"Google\Chrome\User Data\Default\Cache",
            r"Google\Chrome\User Data\Default\Network\Cookies",
            r"Microsoft\Edge\User Data\Default\Cache",
            r"Microsoft\Edge\User Data\Profile 1\History",
            r"Microsoft\Edge\User Data\ShaderCache",
            r"Mozilla\Firefox\Profiles\x.default-release",
        ] {
            let p = env.local.join(rel);
            assert!(is_deletable(&p) && is_trashable(&p), "{}", p.display());
        }
        let p = env
            .roaming
            .join(r"Mozilla\Firefox\Profiles\x.default-release\cookies.sqlite");
        assert!(is_deletable(&p), "{}", p.display());
        for root in ["Microsoft", "Google"] {
            assert!(!is_deletable(&env.local.join(root)), "{root}");
        }
    }

    #[test]
    fn only_caches_follow_the_trash_preference() {
        assert!(!DataKind::Cache.permanent());
        for kind in [
            DataKind::Cookies,
            DataKind::History,
            DataKind::WebsiteData,
            DataKind::FormData,
        ] {
            assert!(kind.permanent(), "{kind:?}");
        }
    }

    /// Read-only smoke test against this PC: `cargo test -p nera-privacy -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_machine_scan_is_read_only() {
        let r = scan(
            &Env::real(),
            &running_executables(),
            &Cancel::new(),
            &|_| {},
        );
        for g in &r.groups {
            println!(
                "{} ({}) installed={} running={} {}",
                g.name,
                g.detail,
                g.installed,
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
