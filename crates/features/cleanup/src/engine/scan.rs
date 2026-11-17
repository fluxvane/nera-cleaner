//! Sizing every category (port of `ScannerActor.scanCategories`).

use super::items::removal_items;
use super::overlap::PathIndex;
use super::size::{item_bytes, Removal, Sizer};
use super::targets::Category;
use super::Progress;
use nera_core::fs_util::Cancel;
use rayon::prelude::*;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// Preferences that shape a scan and a cleanup.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Also remove dotfiles directly inside a target (never inside the Trash,
    /// which is always emptied completely).
    pub include_hidden: bool,
    /// Unused by category sizing: a cleanup removes whole items at any depth,
    /// so the size always counts everything inside them.
    pub max_depth: usize,
    /// "Move to Trash" preference: decides how ordinary cache contents are removed.
    pub to_trash: bool,
    /// Folders the user never wants touched (Settings).
    pub excluded: Vec<PathBuf>,
    /// Paths no broad category may sweep up (see `overlap::reserved_paths`).
    pub reserved: Vec<PathBuf>,
}

impl Options {
    pub fn index(&self, categories: &[Category]) -> PathIndex {
        let mut extra = self.reserved.clone();
        extra.extend(self.excluded.iter().cloned());
        PathIndex::build(categories, &extra)
    }
}

#[derive(Clone, Debug)]
pub struct Scanned {
    pub category: Category,
    /// Bytes a cleanup frees.
    pub size: u64,
    /// Only measured when `size` is 0: bytes that are there but that this user
    /// may not remove (system-owned files). 0 means the category is really empty.
    pub blocked: u64,
}

#[derive(Clone, Debug)]
pub struct ScanOutcome {
    /// Every category (zero-size included) so cleaning can see all owners.
    pub categories: Vec<Scanned>,
    /// Targets that exist but could not be read (Full Disk Access missing).
    pub denied: Vec<PathBuf>,
    pub elapsed: Duration,
    pub cancelled: bool,
}

impl ScanOutcome {
    pub fn total(&self) -> u64 {
        self.categories.iter().map(|c| c.size).sum()
    }
}

/// Size every category in parallel. Per-path errors never stop the scan.
pub fn scan(
    categories: Vec<Category>,
    opts: &Options,
    cancel: &Cancel,
    progress: impl Fn(Progress) + Sync,
) -> ScanOutcome {
    let started = Instant::now();
    let index = opts.index(&categories);
    let total = categories.len();
    let done = AtomicUsize::new(0);
    progress(Progress {
        done: 0,
        total,
        current: String::new(),
    });

    let results: Vec<(Scanned, Vec<PathBuf>)> = categories
        .into_par_iter()
        .map(|category| {
            let (size, blocked, denied) = size_category(&category, &index, opts, cancel);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            progress(Progress {
                done: n,
                total,
                current: category.name.to_string(),
            });
            (
                Scanned {
                    category,
                    size,
                    blocked,
                },
                denied,
            )
        })
        .collect();

    let mut denied = Vec::new();
    let mut categories = Vec::with_capacity(results.len());
    for (scanned, d) in results {
        denied.extend(d);
        categories.push(scanned);
    }
    ScanOutcome {
        categories,
        denied,
        elapsed: started.elapsed(),
        cancelled: cancel.is_cancelled(),
    }
}

/// Size again only the categories named in `ids`, with the exclusions of ALL
/// known categories (a subset alone would double count nested paths).
pub fn rescan(
    all: &[Category],
    ids: &[&str],
    opts: &Options,
    cancel: &Cancel,
) -> Vec<(&'static str, u64, u64)> {
    let index = opts.index(all);
    all.iter()
        .filter(|c| ids.contains(&c.id))
        .map(|c| {
            let (size, blocked, _) = size_category(c, &index, opts, cancel);
            (c.id, size, blocked)
        })
        .collect()
}

/// Bytes cleaning `category` would free, the bytes there that this user may
/// not remove (only measured when nothing is removable), and the folders of
/// this user that could not be read.
fn size_category(
    category: &Category,
    index: &PathIndex,
    opts: &Options,
    cancel: &Cancel,
) -> (u64, u64, Vec<PathBuf>) {
    let mut denied = Vec::new();
    let mut items: Vec<(PathBuf, Removal)> = Vec::new();
    for path in &category.paths {
        if cancel.is_cancelled() {
            break;
        }
        if unreadable_own_dir(path) {
            denied.push(path.clone());
            continue;
        }
        let skip = index.descendant_exclusions(path, category.id);
        items.extend(removal_items(category, path, &skip, opts));
    }
    // Items are independent, so big categories (many cache folders) use every core.
    let sizer = items
        .par_iter()
        .map(|(item, removal)| {
            let mut sizer = Sizer::new(cancel);
            if !cancel.is_cancelled() {
                sizer.add_item(item, *removal);
            }
            sizer
        })
        .reduce(
            || Sizer::new(cancel),
            |mut a, b| {
                a.merge(b);
                a
            },
        );
    denied.extend(sizer.denied.iter().cloned());
    let size = sizer.total();
    let blocked = if size == 0 && !cancel.is_cancelled() {
        items
            .par_iter()
            .map(|(item, removal)| {
                let mut sizer = Sizer::blocked(cancel);
                sizer.add_item(item, *removal);
                sizer.total()
            })
            .sum()
    } else {
        0
    };
    (size, blocked, denied)
}

/// A folder of this user that exists but cannot be listed (privacy
/// protection: Full Disk Access can fix it). Folders of other accounts are
/// never readable and are not reported.
#[cfg(target_os = "macos")]
fn unreadable_own_dir(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    // SAFETY: no arguments, cannot fail.
    let me = unsafe { libc::geteuid() };
    meta.is_dir()
        && meta.uid() == me
        && std::fs::read_dir(path).is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
}

/// A target folder that exists but cannot be listed. On Windows that is a
/// system folder that needs an administrator (`Windows\Temp`), reported so the
/// screen says so instead of calling the category empty. The Recycle Bin is
/// measured by the shell, never listed.
#[cfg(windows)]
fn unreadable_own_dir(path: &Path) -> bool {
    !super::targets::is_trash_dir(path)
        && std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
        && std::fs::read_dir(path).is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
}

/// Installer types the Old installers category may touch. Archives
/// (zip/tar) are often user data, so they are excluded on purpose.
pub const INSTALLER_EXTENSIONS: [&str; 2] = ["dmg", "pkg"];
pub const INSTALLER_MIN_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// `.dmg` / `.pkg` files directly inside `dir` whose newest timestamp
/// (modified, created or status change) is at least 30 days before `now`.
pub fn old_installers(dir: &Path, now: SystemTime) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let ext_ok = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .is_some_and(|e| INSTALLER_EXTENSIONS.contains(&e.as_str()));
        if !ext_ok {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        // .pkg can be a bundle directory; .dmg is a file.
        if meta.file_type().is_symlink() {
            continue;
        }
        #[cfg(unix)]
        let ctime = SystemTime::UNIX_EPOCH + Duration::from_secs(meta.ctime().max(0) as u64);
        // Windows keeps no status-change time; the modification time stands in.
        #[cfg(windows)]
        let ctime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let newest = [meta.modified().ok(), meta.created().ok(), Some(ctime)]
            .into_iter()
            .flatten()
            .max();
        let Some(newest) = newest else { continue };
        if now.duration_since(newest).unwrap_or_default() < INSTALLER_MIN_AGE {
            continue;
        }
        let size = item_bytes(&path, Removal::Trash, &Cancel::new());
        out.push((path, size));
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::targets::{Group, Mode};

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nera-scan-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cat(id: &'static str, paths: Vec<PathBuf>) -> Category {
        Category {
            id,
            name: id,
            description: "",
            icon: "folder",
            group: Group::Caches,
            paths,
            safe: true,
            mode: Mode::Contents,
        }
    }

    fn write(p: &Path, n: usize) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, vec![7u8; n]).unwrap();
    }

    #[test]
    fn counts_nested_paths_once() {
        let root = tmp("overlap");
        write(&root.join("Caches/app/a"), 100_000);
        write(&root.join("Caches/Google/Chrome/b"), 200_000);
        write(&root.join("Caches/Google/Other/c"), 50_000);
        write(&root.join("Caches/.hidden/d"), 50_000);
        let cats = vec![
            cat("user", vec![root.join("Caches")]),
            cat("chrome", vec![root.join("Caches/Google/Chrome")]),
        ];
        let opts = Options {
            include_hidden: false,
            max_depth: 15,
            ..Default::default()
        };
        let out = scan(cats, &opts, &Cancel::new(), |_| {});
        let user = out
            .categories
            .iter()
            .find(|c| c.category.id == "user")
            .unwrap()
            .size;
        let chrome = out
            .categories
            .iter()
            .find(|c| c.category.id == "chrome")
            .unwrap()
            .size;
        // User skips the whole Google folder (and hidden files).
        assert!((100_000..150_000).contains(&user), "{user}");
        assert!((200_000..250_000).contains(&chrome), "{chrome}");
        assert!(!out.cancelled);

        let with_hidden = Options {
            include_hidden: true,
            max_depth: 15,
            ..Default::default()
        };
        let out = scan(
            vec![cat("user", vec![root.join("Caches")])],
            &with_hidden,
            &Cancel::new(),
            |_| {},
        );
        assert!(out.total() >= 400_000);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn respects_user_exclusions_and_missing_paths() {
        let root = tmp("excluded");
        write(&root.join("Caches/keep/a"), 100_000);
        write(&root.join("Caches/skip/deep/b"), 100_000);
        let opts = Options {
            max_depth: 15,
            excluded: vec![root.join("Caches/skip/deep")],
            ..Default::default()
        };
        let out = scan(
            vec![cat("user", vec![root.join("Caches"), root.join("missing")])],
            &opts,
            &Cancel::new(),
            |_| {},
        );
        assert!((100_000..150_000).contains(&out.total()));
        assert!(out.denied.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finds_only_old_installers() {
        let root = tmp("installers");
        write(&root.join("app.dmg"), 10_000);
        write(&root.join("tool.PKG"), 10_000);
        write(&root.join("photo.zip"), 10_000);
        let later = SystemTime::now() + INSTALLER_MIN_AGE + Duration::from_secs(60);
        let found = old_installers(&root, later);
        assert_eq!(found.len(), 2);
        assert!(old_installers(&root, SystemTime::now()).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn other_accounts_private_folders_are_not_a_full_disk_access_problem() {
        use std::os::unix::fs::PermissionsExt;
        // Another account's per-user temp folder: unreadable, and not ours.
        let foreign = Path::new("/private/var/folders/zz/zyxvpxvq6csfxvn_n0000044000011/T");
        if foreign.exists() && std::fs::read_dir(foreign).is_err() {
            assert!(!unreadable_own_dir(foreign));
        }
        // Our own unreadable folder is reported (privacy protection looks the same).
        let root = tmp("fda");
        write(&root.join("locked/x"), 10);
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let out = scan(
            vec![cat("locked", vec![root.join("locked")])],
            &Options::default(),
            &Cancel::new(),
            |_| {},
        );
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert_eq!(out.denied, vec![root.join("locked")]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn system_folders_that_need_an_administrator_are_reported() {
        // Read-only: the Recycle Bin is queried, Windows\Temp only listed.
        let bin = nera_core::paths::system_drive().join("$Recycle.Bin");
        assert!(!unreadable_own_dir(&bin));
        let out = scan(
            vec![cat("trash", vec![bin])],
            &Options::default(),
            &Cancel::new(),
            |_| {},
        );
        assert!(out.denied.is_empty());
        let windows_temp = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("Temp");
        if std::fs::read_dir(&windows_temp).is_err() {
            let out = scan(
                vec![cat("windows-temp", vec![windows_temp.clone()])],
                &Options::default(),
                &Cancel::new(),
                |_| {},
            );
            assert_eq!(out.denied, vec![windows_temp]);
            assert_eq!(out.total(), 0);
        }
        // An ordinary folder of yours is never "denied".
        let root = tmp("readable");
        assert!(!unreadable_own_dir(&root));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancel_stops_early() {
        let root = tmp("cancel");
        write(&root.join("a"), 10_000);
        let cancel = Cancel::new();
        cancel.cancel();
        let out = scan(
            vec![cat("a", vec![root.clone()])],
            &Options::default(),
            &cancel,
            |_| {},
        );
        assert!(out.cancelled);
        assert_eq!(out.total(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }
}
