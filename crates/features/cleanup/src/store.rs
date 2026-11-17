//! Scan and cleanup state shared by the Dashboard and System Junk screens.
//!
//! One `Entity<ScanStore>` lives in a GPUI global, created on first use, so a
//! Smart Scan started on the Dashboard shows its results on System Junk and
//! vice versa. All blocking work runs through `nera_ui::run_blocking`.

use crate::engine::{
    self, CleanReport, CloudDrive, Disposal, LastCleanup, Options, Progress, ScanOutcome, Scanned,
    Status, SystemAsset,
};
use crate::present::bytes;
use gpui_kit::{AnyWindowHandle, App, AppContext as _, Context, Entity, Global, Task, Window};
use nera_core::disk::{self, DiskSpace};
use nera_core::format;
use nera_core::fs_util::Cancel;
use nera_core::os::TRASH;
use nera_ui::{confirm, notify, run_blocking, state, system_notify, Confirm, Shared, Tone};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Scanning,
    Cleaning,
}

/// Outcome of the latest finished scan.
#[derive(Clone, Debug)]
pub struct ScanResults {
    /// Every category, empty ones included.
    pub categories: Vec<Scanned>,
    /// Folders that exist but could not be read.
    pub denied: Vec<PathBuf>,
    pub full_disk_access: Option<bool>,
    /// Seconds since the Unix epoch.
    pub scanned_at: u64,
    pub elapsed: Duration,
}

impl ScanResults {
    pub fn total(&self) -> u64 {
        self.categories.iter().map(|c| c.size).sum()
    }

    pub fn safe_total(&self) -> u64 {
        self.categories
            .iter()
            .filter(|c| c.category.safe)
            .map(|c| c.size)
            .sum()
    }

    pub fn non_empty(&self) -> usize {
        self.categories.iter().filter(|c| c.size > 0).count()
    }

    /// Full Disk Access is missing, or the scan found suspiciously little
    /// because protected folders could not be read. Never on Windows, which
    /// has no such permission: there `present::notes` names the folders that
    /// need an administrator.
    pub fn needs_full_disk_access(&self) -> bool {
        cfg!(target_os = "macos")
            && (self.full_disk_access == Some(false) || !self.denied.is_empty())
    }
}

pub struct ScanStore {
    phase: Phase,
    results: Option<ScanResults>,
    selected: HashSet<&'static str>,
    scan_progress: Shared<Progress>,
    clean_progress: Shared<Progress>,
    cancel: Cancel,
    last_cleanup: Option<LastCleanup>,
    report: Option<CleanReport>,
    /// Free space on the startup disk right before and right after the last
    /// cleanup, as measured (not computed from the sizes of what was removed).
    measured: Option<(u64, u64)>,
    /// Cloud-drive files stored on this disk (never cleaned, only shown).
    cloud: Vec<CloudDrive>,
    /// Data of system features (Siri...): owned by the OS, only shown.
    system_assets: Vec<SystemAsset>,
    disk: Option<DiskSpace>,
    auto_scan_checked: bool,
    _task: Option<Task<()>>,
}

struct StoreGlobal(Entity<ScanStore>);

impl Global for StoreGlobal {}

/// The shared store, created on first use.
pub fn scan_store(cx: &mut App) -> Entity<ScanStore> {
    if let Some(global) = cx.try_global::<StoreGlobal>() {
        return global.0.clone();
    }
    let store = cx.new(|_| ScanStore::new());
    cx.set_global(StoreGlobal(store.clone()));
    store
}

fn options(cx: &App) -> Options {
    let s = state::settings(cx);
    Options {
        include_hidden: s.scan_hidden_files,
        max_depth: s.scan_depth(),
        to_trash: s.move_to_trash,
        excluded: s.excluded_paths.clone(),
        reserved: engine::reserved_paths(),
    }
}

impl ScanStore {
    fn new() -> Self {
        Self {
            phase: Phase::Idle,
            results: None,
            selected: HashSet::new(),
            scan_progress: Shared::new(Progress::default()),
            clean_progress: Shared::new(Progress::default()),
            cancel: Cancel::new(),
            last_cleanup: LastCleanup::load(),
            report: None,
            measured: None,
            cloud: Vec::new(),
            system_assets: Vec::new(),
            disk: disk::startup_volume().ok().map(|v| v.space),
            auto_scan_checked: false,
            _task: None,
        }
    }

    // MARK: Read access

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn is_busy(&self) -> bool {
        self.phase != Phase::Idle
    }

    pub fn results(&self) -> Option<&ScanResults> {
        self.results.as_ref()
    }

    /// Free space before and after the cleanup the report is about.
    pub fn measured(&self) -> Option<(u64, u64)> {
        self.report.as_ref().and(self.measured)
    }

    pub fn cloud_drives(&self) -> &[CloudDrive] {
        &self.cloud
    }

    pub fn system_assets(&self) -> &[SystemAsset] {
        &self.system_assets
    }

    /// Size of the Trash as last measured (0 when unknown or empty).
    pub fn trash_bytes(&self) -> u64 {
        self.results
            .iter()
            .flat_map(|r| r.categories.iter())
            .filter(|s| s.category.empties_trash())
            .map(|s| s.size)
            .sum()
    }

    pub fn report(&self) -> Option<&CleanReport> {
        self.report.as_ref()
    }

    pub fn last_cleanup(&self) -> Option<LastCleanup> {
        self.last_cleanup
    }

    pub fn disk(&self) -> Option<DiskSpace> {
        self.disk
    }

    pub fn scan_progress(&self) -> Progress {
        self.scan_progress.get()
    }

    pub fn clean_progress(&self) -> Progress {
        self.clean_progress.get()
    }

    pub fn is_selected(&self, id: &str) -> bool {
        self.selected.contains(id)
    }

    pub fn selected_count(&self) -> usize {
        self.selection_totals().0
    }

    pub fn selected_bytes(&self) -> u64 {
        self.selection_totals().1
    }

    fn selection_totals(&self) -> (usize, u64) {
        self.results.as_ref().map_or((0, 0), |r| {
            crate::present::selection_totals(&r.categories, &self.selected)
        })
    }

    fn selected_scanned(&self) -> impl Iterator<Item = &Scanned> {
        self.results
            .iter()
            .flat_map(|r| r.categories.iter())
            .filter(|s| s.size > 0 && self.selected.contains(s.category.id))
    }

    pub fn disposal(&self, move_to_trash: bool) -> Disposal {
        let cats: Vec<&engine::Category> = self.selected_scanned().map(|s| &s.category).collect();
        Disposal::of(&cats, move_to_trash)
    }

    // MARK: Selection

    pub fn set_selected(&mut self, id: &'static str, on: bool, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        if on {
            self.selected.insert(id);
        } else {
            self.selected.remove(id);
        }
        cx.notify();
    }

    pub fn select_safe(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        if let Some(r) = &self.results {
            self.selected = engine::safe_selection(&r.categories);
        }
        cx.notify();
    }

    pub fn select_none(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        self.selected.clear();
        cx.notify();
    }

    pub fn dismiss_report(&mut self, cx: &mut Context<Self>) {
        self.report = None;
        cx.notify();
    }

    pub fn refresh_disk(&mut self) {
        self.disk = disk::startup_volume().ok().map(|v| v.space);
    }

    // MARK: Scan

    /// Run Smart Scan on launch when the preference asks for it (once).
    pub fn auto_scan_if_enabled(&mut self, cx: &mut Context<Self>) {
        if self.auto_scan_checked {
            return;
        }
        self.auto_scan_checked = true;
        if state::settings(cx).auto_scan_on_launch && self.results.is_none() {
            self.start_scan(cx);
        }
    }

    pub fn start_scan(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let opts = options(cx);
        let progress = self.scan_progress.clone();
        progress.set(Progress::default());
        let cancel = Cancel::new();
        self.cancel = cancel.clone();
        self.phase = Phase::Scanning;
        self.report = None;
        self._task = Some(run_blocking(
            cx,
            move || {
                let categories = engine::all_categories();
                let outcome = engine::scan(categories, &opts, &cancel, |p| progress.set(p));
                let cloud = engine::cloud_drives(&cancel);
                let assets = engine::system_assets(&cancel);
                (outcome, engine::full_disk_access(), cloud, assets)
            },
            |this, (outcome, fda, cloud, assets), cx| {
                if !outcome.cancelled {
                    this.cloud = cloud;
                    this.system_assets = assets;
                }
                this.finish_scan(outcome, fda, cx)
            },
        ));
        cx.notify();
    }

    fn finish_scan(&mut self, outcome: ScanOutcome, fda: Option<bool>, _cx: &mut Context<Self>) {
        self.phase = Phase::Idle;
        self._task = None;
        self.refresh_disk();
        if outcome.cancelled {
            // Keep whatever the previous scan found.
            return;
        }
        self.selected = engine::auto_selection(&outcome.categories);
        self.results = Some(ScanResults {
            categories: outcome.categories,
            denied: outcome.denied,
            full_disk_access: fda,
            scanned_at: engine::unix_now(),
            elapsed: outcome.elapsed,
        });
    }

    /// Cancel a running scan or cleanup (cooperative).
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        self.cancel.cancel();
        cx.notify();
    }

    pub fn is_cancelling(&self) -> bool {
        self.is_busy() && self.cancel.is_cancelled()
    }

    // MARK: Clean

    /// Ask for confirmation, then clean the selection. Call from a click
    /// handler (not from inside an update of the store itself).
    pub fn request_clean(store: &Entity<ScanStore>, window: &mut Window, cx: &mut App) {
        let (count, bytes, disposal, review) = {
            let s = store.read(cx);
            if s.is_busy() || s.selected_count() == 0 {
                return;
            }
            let review: Vec<&'static str> = s
                .selected_scanned()
                .filter(|c| !c.category.safe)
                .map(|c| c.category.name)
                .collect();
            (
                s.selected_count(),
                s.selected_bytes(),
                s.disposal(state::settings(cx).move_to_trash),
                review,
            )
        };
        let (title, message) = crate::present::confirm_text(count, bytes, &disposal, &review);
        let handle = window.window_handle();
        let store = store.clone();
        confirm(
            Confirm::destructive(title, message, "Clean"),
            true,
            window,
            cx,
            move |_window, cx| {
                store.update(cx, |this, cx| this.start_clean(handle, cx));
            },
        );
    }

    /// Ask, then empty the Trash: the step that actually frees the space of
    /// a cleanup that moved its items there. Always asks (it is permanent).
    pub fn request_empty_trash(store: &Entity<ScanStore>, window: &mut Window, cx: &mut App) {
        let size = {
            let s = store.read(cx);
            if s.is_busy() || s.trash_bytes() == 0 {
                return;
            }
            s.trash_bytes()
        };
        let handle = window.window_handle();
        let store = store.clone();
        confirm(
            Confirm::destructive(
                format!("Empty the {TRASH}?"),
                format!(
                    "Everything in the {TRASH} ({}) is deleted permanently, including files you put there yourself. This can't be undone.",
                    bytes(size)
                ),
                format!("Empty {TRASH}"),
            ),
            false,
            window,
            cx,
            move |_window, cx| {
                store.update(cx, |this, cx| this.start_clean_of(true, handle, cx));
            },
        );
    }

    fn start_clean(&mut self, window: AnyWindowHandle, cx: &mut Context<Self>) {
        self.start_clean_of(false, window, cx)
    }

    /// Clean the selection, or only the Trash when `trash_only`.
    fn start_clean_of(
        &mut self,
        trash_only: bool,
        window: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        if self.is_busy() {
            return;
        }
        let Some(results) = &self.results else { return };
        let all: Vec<engine::Category> = results
            .categories
            .iter()
            .map(|s| s.category.clone())
            .collect();
        let selected: Vec<engine::Category> = if trash_only {
            results
                .categories
                .iter()
                .filter(|s| s.category.empties_trash() && s.size > 0)
                .map(|s| s.category.clone())
                .collect()
        } else {
            self.selected_scanned()
                .map(|s| s.category.clone())
                .collect()
        };
        if selected.is_empty() {
            return;
        }
        let opts = options(cx);
        let to_trash = state::settings(cx).move_to_trash;
        let progress = self.clean_progress.clone();
        progress.set(Progress::default());
        let cancel = Cancel::new();
        self.cancel = cancel.clone();
        self.phase = Phase::Cleaning;
        self.report = None;
        self._task = Some(run_blocking(
            cx,
            move || {
                let free_before = disk::startup_volume().ok().map(|v| v.space.free);
                let report = engine::clean(&selected, &all, &opts, to_trash, &cancel, |p| {
                    progress.set(p)
                });
                // Measure again what is left, so the list never shows a size that
                // cleaning already changed (items moved to the Trash, files that
                // could not be removed, junk written meanwhile).
                // Categories a cancel left untouched kept their size.
                let touched = |id: &str| {
                    report
                        .outcomes
                        .iter()
                        .any(|o| o.id == id && !(o.status == Status::NotRun && o.removed == 0))
                };
                let ids: Vec<&str> = all
                    .iter()
                    .filter(|c| c.empties_trash() || touched(c.id))
                    .map(|c| c.id)
                    .collect();
                let total = selected.len();
                progress.set(Progress {
                    done: total,
                    total,
                    current: "Measuring what is left".to_string(),
                });
                let sizes = engine::rescan(&all, &ids, &opts, &Cancel::new());
                let last = (report.items_removed() > 0).then(|| {
                    LastCleanup::now(
                        report.bytes_freed(),
                        report.categories_cleaned(),
                        report.bytes_trashed(),
                    )
                });
                if let Some(last) = &last {
                    if let Err(err) = last.save() {
                        log::warn!("could not save last cleanup: {err}");
                    }
                }
                let free_after = disk::startup_volume().ok().map(|v| v.space.free);
                (report, last, sizes, free_before.zip(free_after))
            },
            move |this, (report, last, sizes, measured), cx| {
                this.measured = measured;
                this.finish_clean(report, last, sizes, window, cx)
            },
        ));
        cx.notify();
    }

    fn finish_clean(
        &mut self,
        report: CleanReport,
        last: Option<LastCleanup>,
        sizes: Vec<(&'static str, u64, u64)>,
        window: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        self.phase = Phase::Idle;
        self._task = None;
        if let Some(results) = self.results.as_mut() {
            for (id, size, blocked) in sizes {
                if let Some(entry) = results.categories.iter_mut().find(|s| s.category.id == id) {
                    entry.size = size;
                    entry.blocked = blocked;
                }
            }
            for outcome in &report.outcomes {
                if matches!(outcome.status, Status::Cleaned | Status::Covered) {
                    self.selected.remove(outcome.id);
                }
            }
        }
        if last.is_some() {
            self.last_cleanup = last;
        }
        self.refresh_disk();

        let (tone, title, message) = summarize(&report);
        if report.items_removed() > 0 {
            system_notify(cx, &title, &message);
        }
        self.report = Some(report);
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| notify(window, cx, tone, title, message));
        });
    }
}

/// "1 category" / "3 categories".
pub fn categories(n: usize) -> String {
    if n == 1 {
        "1 category".into()
    } else {
        format!("{n} categories")
    }
}

/// Tone, title and message for the toast after a cleanup.
pub fn summarize(report: &CleanReport) -> (Tone, String, String) {
    let issues = report.issues();
    let in_use = report.items_in_use();
    let total = report.bytes_freed();
    let trashed = report.bytes_trashed();
    let deleted = total.saturating_sub(trashed);
    let items = format::count(report.items_removed(), "item");
    let title = if report.items_removed() == 0 {
        if report.cancelled {
            "Cleanup cancelled".to_string()
        } else {
            "Nothing was removed".to_string()
        }
    } else if trashed == 0 {
        format!("Freed {}", bytes(total))
    } else if deleted == 0 {
        format!("Moved {} to the {TRASH}", bytes(total))
    } else {
        format!(
            "Freed {} and moved {} to the {TRASH}",
            bytes(deleted),
            bytes(trashed)
        )
    };
    let mut message = if report.items_removed() == 0 {
        if report.cancelled {
            "You stopped it before anything was removed.".to_string()
        } else if issues.is_empty() && in_use == 0 {
            "Everything selected was already clean.".to_string()
        } else {
            String::new()
        }
    } else if trashed == 0 {
        format!("{items} removed.")
    } else if deleted == 0 {
        format!("{items} moved. Empty the {TRASH} to free the space.")
    } else {
        format!(
            "{items} removed. Empty the {TRASH} to free the {} moved there.",
            bytes(trashed)
        )
    };
    if report.cancelled && report.items_removed() > 0 {
        message.push_str(" You stopped it before the rest was cleaned.");
    }
    if in_use > 0 {
        if !message.is_empty() {
            message.push(' ');
        }
        message.push_str(&format!("{}.", engine::in_use_sentence(in_use)));
    }
    if !issues.is_empty() {
        let running = report.running_ides();
        if !message.is_empty() {
            message.push(' ');
        }
        let verb = if issues.len() == 1 { "needs" } else { "need" };
        message.push_str(&format!("{} {verb} attention.", categories(issues.len())));
        if !running.is_empty() {
            message.push_str(&format!(" Quit {} and clean again.", running.join(", ")));
        }
    }
    let tone = if issues.is_empty() {
        if report.items_removed() == 0 {
            Tone::Info
        } else {
            Tone::Success
        }
    } else if report.items_removed() == 0 {
        Tone::Danger
    } else {
        Tone::Warning
    };
    (tone, title, message.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::CategoryOutcome;

    fn outcome(
        status: Status,
        removed: usize,
        bytes: u64,
        trashed: u64,
        running: &[&str],
    ) -> CategoryOutcome {
        CategoryOutcome {
            id: "x",
            name: "X",
            status,
            removed,
            bytes,
            bytes_trashed: trashed,
            failed: usize::from(status == Status::Failed),
            first_error: None,
            running: running.iter().map(|s| s.to_string()).collect(),
            in_use: 0,
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn summarizes_cleanups() {
        let ok = CleanReport {
            outcomes: vec![outcome(Status::Cleaned, 3, 2_000_000, 2_000_000, &[])],
            cancelled: false,
            moved_to_trash: true,
        };
        let (tone, title, msg) = summarize(&ok);
        assert_eq!(tone, Tone::Success);
        assert_eq!(title, "Moved 2.00 MB to the Trash");
        assert!(msg.contains("Empty the Trash"));

        let partial = CleanReport {
            outcomes: vec![outcome(Status::Partial, 1, 1_000_000, 0, &["Rider2024.1"])],
            cancelled: false,
            moved_to_trash: false,
        };
        let (tone, title, msg) = summarize(&partial);
        assert_eq!(tone, Tone::Warning);
        assert_eq!(title, "Freed 1.00 MB");
        assert!(msg.contains("Rider2024.1") && msg.contains("1 category needs attention"));

        let mixed = CleanReport {
            outcomes: vec![outcome(Status::Cleaned, 2, 3_000_000, 1_000_000, &[])],
            cancelled: false,
            moved_to_trash: true,
        };
        let (_, title, msg) = summarize(&mixed);
        assert_eq!(title, "Freed 2.00 MB and moved 1.00 MB to the Trash");
        assert!(msg.contains("Empty the Trash to free the 1.00 MB"));

        let none = CleanReport {
            outcomes: vec![outcome(Status::SkippedRunning, 0, 0, 0, &["Rider2024.1"])],
            ..Default::default()
        };
        assert_eq!(summarize(&none).0, Tone::Danger);
    }

    #[cfg(windows)]
    #[test]
    fn summarizes_cleanups_with_the_recycle_bin_and_files_in_use() {
        let ok = CleanReport {
            outcomes: vec![outcome(Status::Cleaned, 3, 2_000_000, 2_000_000, &[])],
            cancelled: false,
            moved_to_trash: true,
        };
        let (tone, title, msg) = summarize(&ok);
        assert_eq!(tone, Tone::Success);
        assert_eq!(
            title,
            format!("Moved {} to the Recycle Bin", bytes(2_000_000))
        );
        assert!(msg.contains("Empty the Recycle Bin"));

        let mut busy = outcome(Status::Cleaned, 2, 1_000_000, 0, &[]);
        busy.in_use = 4;
        let report = CleanReport {
            outcomes: vec![busy.clone()],
            ..Default::default()
        };
        let (tone, _, msg) = summarize(&report);
        assert_eq!(tone, Tone::Success, "files in use are not an error");
        assert_eq!(
            msg,
            "2 items removed. 4 items in use by other apps were skipped."
        );

        busy.removed = 0;
        busy.bytes = 0;
        busy.in_use = 1;
        let report = CleanReport {
            outcomes: vec![busy],
            ..Default::default()
        };
        let (tone, title, msg) = summarize(&report);
        assert_eq!((tone, title.as_str()), (Tone::Info, "Nothing was removed"));
        assert_eq!(msg, "1 item in use by other apps was skipped.");
    }
}
