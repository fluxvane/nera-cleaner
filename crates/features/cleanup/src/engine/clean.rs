//! Cleaning selected categories (port of `DeleteFilesUseCase` +
//! `ScannerActor.cleanCategory`). Every removal goes through
//! `nera_core::fs_util`.

use super::items::removal_items;
use super::overlap::{cleaning_order, targets_already_covered};
use super::scan::Options;
use super::size::{item_bytes, Removal};
#[cfg(windows)]
use super::targets::is_trash_dir;
use super::targets::Category;
use super::Progress;
use nera_core::fs_util::{self, Cancel};
use nera_core::paths::{is_under_any, standardize};
use nera_core::safety;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Everything that could be found was removed.
    Cleaned,
    /// Some items were removed, some failed or belong to a running IDE.
    Partial,
    /// Nothing could be removed.
    Failed,
    /// Every target belongs to a running IDE.
    SkippedRunning,
    /// A broader category cleaned in this run already covered it.
    Covered,
    /// The run was cancelled before this category finished.
    NotRun,
}

#[derive(Clone, Debug)]
pub struct CategoryOutcome {
    pub id: &'static str,
    pub name: &'static str,
    pub status: Status,
    pub removed: usize,
    /// Bytes removed or moved to the Trash by this category.
    pub bytes: u64,
    /// The part of `bytes` that went to the Trash (not freed until it is emptied).
    pub bytes_trashed: u64,
    pub failed: usize,
    pub first_error: Option<String>,
    /// Running IDEs whose folders were skipped.
    pub running: Vec<String>,
    /// Items skipped because another program holds them open. Windows refuses
    /// to remove those; it is not an error, so the status is unaffected.
    pub in_use: usize,
}

impl CategoryOutcome {
    /// Needs the user's attention (kept selected, listed in the banner).
    pub fn needs_attention(&self) -> bool {
        matches!(
            self.status,
            Status::Partial | Status::Failed | Status::SkippedRunning
        )
    }

    /// One-line explanation for the issues banner.
    pub fn message(&self) -> Option<String> {
        let mut parts = Vec::new();
        if !self.running.is_empty() {
            parts.push(format!(
                "Skipped while running: {}. Quit it and clean again",
                self.running.join(", ")
            ));
        }
        if self.in_use > 0 {
            parts.push(in_use_sentence(self.in_use));
        }
        if self.failed > 0 {
            let err = self.first_error.as_deref().unwrap_or("unknown error");
            parts.push(format!(
                "{} could not be removed ({err})",
                nera_core::format::count(self.failed, "item")
            ));
        }
        if self.status == Status::Failed
            && self.failed == 0
            && self.running.is_empty()
            && self.in_use == 0
        {
            parts.push(NOTHING_REMOVED.into());
        }
        (!parts.is_empty()).then(|| parts.join(". "))
    }
}

#[derive(Clone, Debug, Default)]
pub struct CleanReport {
    pub outcomes: Vec<CategoryOutcome>,
    pub cancelled: bool,
    pub moved_to_trash: bool,
}

impl CleanReport {
    pub fn bytes_freed(&self) -> u64 {
        self.outcomes.iter().map(|o| o.bytes).sum()
    }

    /// Bytes that went to the Trash instead of being deleted.
    pub fn bytes_trashed(&self) -> u64 {
        self.outcomes.iter().map(|o| o.bytes_trashed).sum()
    }

    pub fn items_removed(&self) -> usize {
        self.outcomes.iter().map(|o| o.removed).sum()
    }

    /// Items skipped because other programs hold them open (Windows).
    pub fn items_in_use(&self) -> usize {
        self.outcomes.iter().map(|o| o.in_use).sum()
    }

    /// Categories that removed something in this run.
    pub fn categories_cleaned(&self) -> usize {
        self.outcomes.iter().filter(|o| o.removed > 0).count()
    }

    pub fn issues(&self) -> Vec<&CategoryOutcome> {
        self.outcomes
            .iter()
            .filter(|o| o.needs_attention())
            .collect()
    }

    /// Categories whose junk is gone (removed from the list).
    pub fn cleaned_ids(&self) -> Vec<&'static str> {
        self.outcomes
            .iter()
            .filter(|o| matches!(o.status, Status::Cleaned | Status::Covered))
            .map(|o| o.id)
            .collect()
    }

    pub fn running_ides(&self) -> Vec<String> {
        let set: BTreeSet<&String> = self
            .outcomes
            .iter()
            .flat_map(|o| o.running.iter())
            .collect();
        set.into_iter().cloned().collect()
    }
}

/// Clean `selected`. `all` is EVERY known category (selected or not) so a broad
/// category never removes a narrower one's folders. `move_to_trash` follows
/// the user's preference for ordinary cache contents.
pub fn clean(
    selected: &[Category],
    all: &[Category],
    opts: &Options,
    move_to_trash: bool,
    cancel: &Cancel,
    progress: impl Fn(Progress),
) -> CleanReport {
    let mut known: Vec<Category> = all.to_vec();
    for c in selected {
        if !known.iter().any(|k| k.id == c.id) {
            known.push(c.clone());
        }
    }
    let opts = &Options {
        to_trash: move_to_trash,
        ..opts.clone()
    };
    let index = opts.index(&known);
    let ordered = cleaning_order(selected);
    let total = ordered.len();
    let mut cleaned_roots: Vec<PathBuf> = Vec::new();
    let mut report = CleanReport {
        moved_to_trash: move_to_trash,
        ..Default::default()
    };

    for (i, category) in ordered.iter().enumerate() {
        let mut outcome = CategoryOutcome {
            id: category.id,
            name: category.name,
            status: Status::NotRun,
            removed: 0,
            bytes: 0,
            bytes_trashed: 0,
            failed: 0,
            first_error: None,
            running: Vec::new(),
            in_use: 0,
        };
        if cancel.is_cancelled() {
            report.cancelled = true;
            report.outcomes.push(outcome);
            continue;
        }
        progress(Progress {
            done: i,
            total,
            current: category.name.to_string(),
        });

        if targets_already_covered(category, &cleaned_roots) {
            outcome.status = Status::Covered;
            report.outcomes.push(outcome);
            continue;
        }

        let running: BTreeSet<String> = category
            .paths
            .iter()
            .filter_map(|p| safety::running_ide_owning(p))
            .collect();
        outcome.running = running.into_iter().collect();

        // Targets cleaned without exclusions; they cover anything below them.
        let mut whole_targets = Vec::new();
        let mut job = Job {
            opts,
            cancel,
            outcome: &mut outcome,
        };
        for target in &category.paths {
            if cancel.is_cancelled() {
                break;
            }
            if is_under_any(target, &cleaned_roots)
                || is_under_any(target, &opts.excluded)
                || safety::running_ide_owning(target).is_some()
            {
                continue;
            }
            let exclusions = index.descendant_exclusions(target, category.id);
            if exclusions.is_empty() {
                whole_targets.push(standardize(target));
            }
            let mut skip = cleaned_roots.clone();
            skip.extend(exclusions);
            job.clean_target(category, target, &skip);
        }

        // Cancel pressed inside this category: it is unfinished, not clean.
        // What it did remove still counts in the totals, and it stays selected.
        let stopped = cancel.is_cancelled();
        outcome.status = if stopped {
            Status::NotRun
        } else if outcome.removed > 0 {
            if outcome.failed > 0 || !outcome.running.is_empty() {
                Status::Partial
            } else {
                Status::Cleaned
            }
        } else if !outcome.running.is_empty() {
            Status::SkippedRunning
        } else if outcome.failed > 0 {
            Status::Failed
        } else {
            // Nothing was there any more (or only protected items).
            Status::Cleaned
        };
        if !stopped && outcome.removed > 0 && outcome.failed == 0 && outcome.in_use == 0 {
            cleaned_roots.extend(whole_targets);
        }
        if stopped {
            report.cancelled = true;
        }
        report.outcomes.push(outcome);
    }
    progress(Progress {
        done: total,
        total,
        current: String::new(),
    });
    report
}

fn is_gone(item: &Path) -> bool {
    std::fs::symlink_metadata(item).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

struct Job<'a> {
    opts: &'a Options,
    cancel: &'a Cancel,
    outcome: &'a mut CategoryOutcome,
}

impl Job<'_> {
    /// Remove every item `removal_items` lists for `target`.
    fn clean_target(&mut self, category: &Category, target: &Path, skip: &[PathBuf]) {
        for (item, removal) in removal_items(category, target, skip, self.opts) {
            if self.cancel.is_cancelled() {
                return;
            }
            // Fresh tokens: a Cancel pressed now must not shorten the size of an
            // item that is removed anyway (the toast would under-report).
            let before = item_bytes(&item, removal, &Cancel::new());
            let res = match removal {
                #[cfg(windows)]
                Removal::Permanent if is_trash_dir(&item) => super::win::empty_recycle_bin(),
                Removal::Trash => fs_util::trash_item(&item),
                Removal::Permanent => fs_util::remove_item(&item),
            };
            // Windows refuses to remove a file another program holds open, and
            // a folder holding one: remove everything else in it.
            #[cfg(windows)]
            let res = match res {
                Err(e) if removal == Removal::Permanent && !is_trash_dir(&item) => {
                    super::win::remove_rest(&item, e)
                }
                res => res,
            };
            // A failed permanent removal may still have deleted part of the item.
            let freed = match (&res, removal) {
                (Ok(()), _) => before,
                (Err(_), Removal::Permanent) => {
                    before.saturating_sub(item_bytes(&item, removal, &Cancel::new()))
                }
                (Err(_), Removal::Trash) => 0,
            };
            self.record(res, freed, removal, &item);
        }
    }

    fn record(&mut self, res: std::io::Result<()>, freed: u64, removal: Removal, item: &Path) {
        self.outcome.bytes += freed;
        match res {
            Ok(()) => {
                self.outcome.removed += 1;
                if removal == Removal::Trash {
                    self.outcome.bytes_trashed += freed;
                }
            }
            // Gone since it was listed (temporary files come and go while a
            // cleanup runs): nothing left to remove, and not a failure.
            Err(_) if is_gone(item) => {}
            Err(e) if in_use(&e, item) => self.outcome.in_use += 1,
            Err(e) => {
                self.outcome.failed += 1;
                if self.outcome.first_error.is_none() {
                    self.outcome.first_error = Some(short_error(&e));
                }
            }
        }
    }
}

/// True when another program holds `item` open. Only Windows refuses to
/// remove open files; on macOS they are removed like any other.
#[cfg(target_os = "macos")]
fn in_use(_err: &std::io::Error, _item: &Path) -> bool {
    false
}
#[cfg(windows)]
use super::win::is_in_use as in_use;

/// "3 items in use by other apps were skipped".
pub fn in_use_sentence(n: usize) -> String {
    let verb = if n == 1 { "was" } else { "were" };
    format!(
        "{} in use by other apps {verb} skipped",
        nera_core::format::count(n, "item")
    )
}

#[cfg(target_os = "macos")]
const NOTHING_REMOVED: &str = "Nothing could be removed. Check Full Disk Access and try again";
#[cfg(windows)]
const NOTHING_REMOVED: &str =
    "Nothing could be removed. Close the apps using these files, or run Nera as administrator for system folders, and try again";

fn short_error(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => "permission denied".into(),
        _ => {
            let s = e.to_string();
            // "Operation not permitted (os error 1)" → "Operation not permitted"
            s.split(" (os error").next().unwrap_or(&s).to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::targets::{Group, Mode};

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nera-clean-{name}-{}", std::process::id()));
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

    fn opts() -> Options {
        Options {
            include_hidden: false,
            max_depth: 15,
            ..Default::default()
        }
    }

    #[test]
    fn broad_category_never_removes_narrow_ones_even_when_unselected() {
        let root = tmp("overlap");
        write(&root.join("Caches/app/a"), 10_000);
        write(&root.join("Caches/Google/Chrome/b"), 10_000);
        write(&root.join("Caches/JetBrains/Rider/caches/c"), 10_000);
        write(&root.join("Caches/.keep"), 10);
        let user = cat("user", vec![root.join("Caches")]);
        let chrome = cat("chrome", vec![root.join("Caches/Google/Chrome")]);
        let jb = cat("jb", vec![root.join("Caches/JetBrains/Rider/caches")]);
        let all = vec![user.clone(), chrome, jb];

        let report = clean(&[user], &all, &opts(), false, &Cancel::new(), |_| {});
        assert!(!root.join("Caches/app").exists());
        assert!(root.join("Caches/Google/Chrome/b").exists());
        assert!(root.join("Caches/JetBrains/Rider/caches/c").exists());
        assert!(
            root.join("Caches/.keep").exists(),
            "hidden files follow the scan setting"
        );
        assert!(
            root.join("Caches").is_dir(),
            "the target folder itself stays"
        );
        assert_eq!(report.outcomes[0].status, Status::Cleaned);
        assert_eq!(report.items_removed(), 1);
        assert!(report.bytes_freed() >= 10_000);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cleans_narrow_first_and_skips_covered() {
        let root = tmp("order");
        write(&root.join("Caches/Google/Chrome/b"), 10_000);
        write(&root.join("Caches/app/a"), 10_000);
        write(&root.join("dot/pip/x"), 10_000);
        let all = vec![
            cat("user", vec![root.join("Caches")]),
            cat("chrome", vec![root.join("Caches/Google/Chrome")]),
            cat("dot", vec![root.join("dot")]),
            cat("pip", vec![root.join("dot/pip")]),
        ];
        let report = clean(&all, &all, &opts(), false, &Cancel::new(), |_| {});
        let ids: Vec<_> = report.outcomes.iter().map(|o| o.id).collect();
        assert_eq!(ids[0], "chrome");
        assert_eq!(
            report
                .outcomes
                .iter()
                .find(|o| o.id == "pip")
                .unwrap()
                .status,
            Status::Cleaned
        );
        assert!(!root.join("Caches/Google/Chrome/b").exists());
        assert!(root.join("Caches/Google/Chrome").is_dir());
        assert!(!root.join("Caches/app").exists());
        assert!(!root.join("dot/pip/x").exists());
        // "dot" excludes the pip folder (another owner), so it stays.
        assert!(root.join("dot/pip").is_dir());
        assert!(report.issues().is_empty(), "{:?}", report.outcomes);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn identical_targets_are_cleaned_once() {
        let root = tmp("covered");
        write(&root.join("pip/x"), 10_000);
        let all = vec![
            cat("a", vec![root.join("pip")]),
            cat("b", vec![root.join("pip")]),
        ];
        let report = clean(&all, &all, &opts(), false, &Cancel::new(), |_| {});
        assert_eq!(report.outcomes[0].status, Status::Cleaned);
        assert_eq!(report.outcomes[1].status, Status::Covered);
        assert_eq!(report.cleaned_ids(), vec!["a", "b"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn empties_trash_dirs_permanently_and_respects_exclusions() {
        let root = tmp("trash");
        write(&root.join("u/.Trash/old.txt"), 1_000);
        write(&root.join("u/.Trash/.hidden"), 1_000);
        write(&root.join("logs/keep/a"), 1_000);
        write(&root.join("logs/drop/a"), 1_000);
        let trash = cat("trash", vec![root.join("u/.Trash")]);
        assert!(trash.empties_trash());
        let logs = cat("logs", vec![root.join("logs")]);
        let all = vec![trash, logs];
        let o = Options {
            excluded: vec![root.join("logs/keep/a")],
            ..opts()
        };
        let report = clean(&all, &all, &o, false, &Cancel::new(), |_| {});
        assert_eq!(report.outcomes[0].id, "trash");
        assert_eq!(std::fs::read_dir(root.join("u/.Trash")).unwrap().count(), 0);
        assert!(root.join("logs/keep/a").exists());
        assert!(!root.join("logs/drop").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn single_file_targets_and_cancel() {
        let root = tmp("file");
        write(&root.join("state.backup"), 5_000);
        let c = cat("file", vec![root.join("state.backup")]);
        let cancel = Cancel::new();
        cancel.cancel();
        let report = clean(
            std::slice::from_ref(&c),
            std::slice::from_ref(&c),
            &opts(),
            false,
            &cancel,
            |_| {},
        );
        assert!(report.cancelled);
        assert_eq!(report.outcomes[0].status, Status::NotRun);
        assert!(root.join("state.backup").exists());

        let report = clean(
            std::slice::from_ref(&c),
            &[],
            &opts(),
            false,
            &Cancel::new(),
            |_| {},
        );
        assert_eq!(report.outcomes[0].status, Status::Cleaned);
        assert!(!root.join("state.backup").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scanned_size_equals_freed_size() {
        use crate::engine::scan::{rescan, scan};
        let root = tmp("equal");
        // Nested deeper than the scan depth, a dotfile inside a normal folder,
        // a top-level dotfile (never removed) and a hard link inside the tree.
        write(&root.join("Caches/app/l1/l2/l3/l4/deep.bin"), 300_000);
        write(&root.join("Caches/app/.inner-hidden"), 200_000);
        write(&root.join("Caches/.top-hidden/kept.bin"), 400_000);
        write(&root.join("Caches/file.bin"), 100_000);
        std::fs::hard_link(
            root.join("Caches/file.bin"),
            root.join("Caches/app/link.bin"),
        )
        .unwrap();
        let cats = vec![cat("user", vec![root.join("Caches")])];
        let shallow = Options {
            include_hidden: false,
            max_depth: 2,
            ..Default::default()
        };
        let scanned = scan(cats.clone(), &shallow, &Cancel::new(), |_| {}).total();
        // 300k + 200k + 100k (the hard-linked file counts once): hidden top-level excluded.
        assert!((600_000..700_000).contains(&scanned), "{scanned}");

        let report = clean(&cats, &cats, &shallow, false, &Cancel::new(), |_| {});
        assert_eq!(report.bytes_freed(), scanned);
        assert_eq!(report.bytes_trashed(), 0);
        assert_eq!(report.categories_cleaned(), 1);
        assert!(root.join("Caches/.top-hidden/kept.bin").exists());
        let left = rescan(&cats, &["user"], &shallow, &Cancel::new());
        assert_eq!(
            left,
            vec![("user", 0, 0)],
            "nothing the scan counted is left"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_targets_are_not_failures() {
        let root = tmp("missing");
        let c = cat("gone", vec![root.join("nope")]);
        let report = clean(
            std::slice::from_ref(&c),
            &[],
            &opts(),
            false,
            &Cancel::new(),
            |_| {},
        );
        assert_eq!(report.outcomes[0].status, Status::Cleaned);
        assert!(report.outcomes[0].message().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn outcome_messages() {
        let mut o = CategoryOutcome {
            id: "x",
            name: "X",
            status: Status::Partial,
            removed: 1,
            bytes: 1,
            bytes_trashed: 0,
            failed: 2,
            first_error: Some("permission denied".into()),
            running: vec!["Rider2024.1".into()],
            in_use: 0,
        };
        let m = o.message().unwrap();
        assert!(m.contains("Rider2024.1") && m.contains("2 items could not be removed"));
        o.status = Status::Failed;
        o.failed = 0;
        o.running.clear();
        assert_eq!(o.message().unwrap(), NOTHING_REMOVED);
        o.in_use = 3;
        assert_eq!(
            o.message().unwrap(),
            "3 items in use by other apps were skipped"
        );
        assert_eq!(
            in_use_sentence(1),
            "1 item in use by other apps was skipped"
        );
    }

    #[cfg(windows)]
    #[test]
    fn files_in_use_are_skipped_not_failures_and_the_rest_goes() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = tmp("inuse");
        write(&root.join(r"Temp\setup\held.dll"), 10_000);
        write(&root.join(r"Temp\setup\other.bin"), 20_000);
        write(&root.join(r"Temp\done\x.bin"), 30_000);
        write(&root.join(r"Temp\loose.log"), 40_000);
        // Opened without FILE_SHARE_DELETE, as most programs do.
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1) // FILE_SHARE_READ
            .open(root.join(r"Temp\setup\held.dll"))
            .unwrap();
        let c = cat("temp", vec![root.join("Temp")]);
        let report = clean(
            std::slice::from_ref(&c),
            std::slice::from_ref(&c),
            &opts(),
            false,
            &Cancel::new(),
            |_| {},
        );
        let o = &report.outcomes[0];
        assert_eq!((o.removed, o.in_use, o.failed), (2, 1, 0), "{o:?}");
        assert_eq!(o.status, Status::Cleaned);
        assert!(!o.needs_attention());
        // Everything but the open file is gone, and its bytes count as freed.
        assert_eq!(report.bytes_freed(), 90_000);
        assert!(root.join(r"Temp\setup\held.dll").exists());
        assert!(!root.join(r"Temp\setup\other.bin").exists());
        assert!(!root.join(r"Temp\done").exists() && !root.join(r"Temp\loose.log").exists());
        assert_eq!(report.items_in_use(), 1);
        drop(lock);
        std::fs::remove_dir_all(root).unwrap();
    }
}
