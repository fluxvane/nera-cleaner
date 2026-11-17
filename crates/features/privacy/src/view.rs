//! Every number and label the Privacy screen shows, as pure functions of the
//! scan result. The screen renders these strings verbatim and
//! `examples/dump.rs` prints them, so what is checked is what is shown.

use crate::engine::{Access, CleanReport, DataKind, Group, Item, ScanResult};
use nera_core::format;
use std::collections::HashSet;

/// Why a row can or cannot be ticked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowState {
    Ready,
    /// Something was measured, but part of the data lives in a folder macOS
    /// hides without Full Disk Access, so the size is a lower bound.
    Partial,
    Empty,
    NoAccess,
    BrowserRunning,
}

impl RowState {
    /// Rows in this state can be ticked.
    pub fn selectable(self) -> bool {
        matches!(self, RowState::Ready | RowState::Partial)
    }
}

/// Part of this item may be invisible to us: it lives in a folder macOS
/// protects and Full Disk Access is off.
fn hidden(item: &Item, access: Option<Access>) -> bool {
    item.needs_full_disk_access && access == Some(Access::Missing)
}

/// What was measured for an item, ignoring whether its browser is open.
/// "Empty" is only claimed when nothing is hidden from us; with Full Disk
/// Access off, a protected location that measured 0 is "No access", and one
/// that measured something is a lower bound.
pub fn availability(item: &Item, access: Option<Access>) -> RowState {
    let hidden = hidden(item, access);
    if item.bytes > 0 {
        if hidden {
            RowState::Partial
        } else {
            RowState::Ready
        }
    } else if hidden {
        RowState::NoAccess
    } else {
        RowState::Empty
    }
}

/// State of one item row (an open browser blocks selection).
pub fn row_state(group: &Group, item: &Item, access: Option<Access>) -> RowState {
    if group.running {
        RowState::BrowserRunning
    } else {
        availability(item, access)
    }
}

/// The size column of a row. Shown even while the browser is open.
pub fn row_value(item: &Item, access: Option<Access>) -> String {
    match availability(item, access) {
        RowState::Empty => "Empty".to_string(),
        RowState::NoAccess => "No access".to_string(),
        RowState::Partial => format!("At least {}", format::bytes(item.bytes)),
        RowState::Ready | RowState::BrowserRunning => format::bytes(item.bytes),
    }
}

/// Subtitle under a row label.
pub fn row_subtitle(kind: DataKind, to_trash: bool) -> String {
    match kind {
        DataKind::Cache if to_trash => {
            format!(
                "{}. Moved to the {}",
                kind.description(),
                nera_core::os::TRASH
            )
        }
        _ => kind.description().to_string(),
    }
}

/// Section description: `"[App not installed · ]<detail>[ · <size>]"`; the
/// size is omitted while nothing is measured.
pub fn group_description(group: &Group, access: Option<Access>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !group.installed {
        parts.push("App not installed, data left behind".to_string());
    }
    parts.push(group.detail.clone());
    let size = group.bytes();
    if size > 0 {
        let lower_bound = group.items.iter().any(|i| i.bytes > 0 && hidden(i, access));
        parts.push(if lower_bound {
            format!("at least {}", format::bytes(size))
        } else {
            format::bytes(size)
        });
    }
    parts.join(" \u{b7} ")
}

/// The three stat tiles at the top of the screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub browser_bytes: u64,
    /// Browsers whose app is installed (plus Safari, which ships with macOS).
    pub installed_browsers: usize,
    /// Browsers listed only because their data is still on disk.
    pub leftover_browsers: usize,
    /// Some protected data could not be measured, so the browser total is a lower bound.
    pub incomplete: bool,
    /// The scan was cancelled: browsers and sizes after the stop point are missing.
    pub scan_stopped: bool,
    pub recent_bytes: u64,
    pub selected_bytes: u64,
    pub selected_items: usize,
}

pub fn summary(groups: &[Group], selected: &HashSet<String>, access: Option<Access>) -> Summary {
    let browsers = || groups.iter().filter(|g| g.id != "recent");
    let chosen = || {
        groups
            .iter()
            .flat_map(|g| g.items.iter())
            .filter(|i| selected.contains(&i.id))
    };
    Summary {
        browser_bytes: browsers().map(Group::bytes).sum(),
        installed_browsers: browsers().filter(|g| g.installed).count(),
        leftover_browsers: browsers().filter(|g| !g.installed).count(),
        incomplete: browsers().any(|g| g.items.iter().any(|i| hidden(i, access))),
        scan_stopped: false,
        recent_bytes: groups
            .iter()
            .filter(|g| g.id == "recent")
            .map(Group::bytes)
            .sum(),
        selected_bytes: chosen().map(|i| i.bytes).sum(),
        selected_items: chosen().count(),
    }
}

impl Summary {
    /// Mark the summary as built from a scan that was stopped early.
    pub fn scan_stopped(mut self, stopped: bool) -> Self {
        self.scan_stopped = stopped;
        self
    }

    pub fn browser_value(&self) -> String {
        format::bytes(self.browser_bytes)
    }

    pub fn browser_caption(&self) -> String {
        if self.scan_stopped {
            return "Scan stopped early \u{b7} partial".to_string();
        }
        let mut caption = format!(
            "{} installed",
            format::count(self.installed_browsers, "supported browser")
        );
        if self.leftover_browsers > 0 {
            caption.push_str(&format!(
                " \u{b7} {} with leftover data",
                self.leftover_browsers
            ));
        }
        if self.incomplete {
            caption.push_str(" \u{b7} protected data not counted");
        }
        caption
    }

    pub fn recent_value(&self) -> String {
        format::bytes(self.recent_bytes)
    }

    pub fn selected_value(&self) -> String {
        format::bytes(self.selected_bytes)
    }

    pub fn selected_caption(&self) -> String {
        format::count(self.selected_items, "item")
    }
}

/// Footer text: `"Nothing selected"` or `"3 items selected · 12.3 MB"`.
pub fn footer_summary(s: &Summary) -> String {
    if s.selected_items == 0 {
        "Nothing selected".to_string()
    } else {
        format!(
            "{} selected · {}",
            format::count(s.selected_items, "item"),
            s.selected_value()
        )
    }
}

pub fn clean_button_label(s: &Summary) -> String {
    if s.selected_items == 0 {
        "Clean selected".to_string()
    } else {
        format!("Clean {}", s.selected_value())
    }
}

/// Names of browsers that are open, for the banner.
pub fn running_names(result: &ScanResult) -> Vec<&str> {
    result
        .groups
        .iter()
        .filter(|g| g.running)
        .map(|g| g.name.as_str())
        .collect()
}

/// `"Safari is open"` / `"Safari, Chrome are open"`.
pub fn running_title(names: &[&str]) -> String {
    format!(
        "{} {} open",
        names.join(", "),
        if names.len() == 1 { "is" } else { "are" }
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Danger,
    Success,
}

/// What to tell the user once a cleanup has finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanOutcome {
    pub severity: Severity,
    pub title: String,
    pub message: String,
}

/// Space is "freed" only for what was deleted; what went to the Trash still
/// occupies the disk until the Trash is emptied.
pub fn clean_outcome(report: &CleanReport) -> CleanOutcome {
    let n = report.cleaned.len();
    let failed = report.failed.len();
    let mut details = Vec::new();
    if report.freed > 0 {
        details.push(format!("Freed {}.", format::bytes(report.freed)));
    }
    if report.trashed_bytes > 0 {
        details.push(format!(
            "Moved {} to the {}. The space is freed when you empty it.",
            format::bytes(report.trashed_bytes),
            nera_core::os::TRASH
        ));
    } else if report.trashed > 0 {
        details.push(format!("Caches are in the {}.", nera_core::os::TRASH));
    }
    if !report.skipped_running.is_empty() {
        details.push(format!(
            "Skipped {}: quit it first.",
            report.skipped_running.join(", ")
        ));
    }
    if let Some((path, why)) = report.failed.first() {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        details.push(format!(
            "{} could not be removed ({name}: {why}).",
            format::count(failed, "file")
        ));
    }
    if report.cancelled {
        details.push("The cleanup was stopped.".to_string());
    }
    let (severity, title) = if n == 0 && report.removed == 0 {
        (
            if failed > 0 {
                Severity::Danger
            } else {
                Severity::Info
            },
            "Nothing was cleaned".to_string(),
        )
    } else if failed > 0 || !report.skipped_running.is_empty() || report.cancelled {
        (
            Severity::Warning,
            format!("Cleaned {}", format::count(n, "item")),
        )
    } else {
        (
            Severity::Success,
            format!("Cleaned {}", format::count(n, "item")),
        )
    };
    CleanOutcome {
        severity,
        title,
        message: details.join(" "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, kind: DataKind, bytes: u64, fda: bool) -> Item {
        Item {
            id: id.into(),
            kind,
            targets: Vec::new(),
            bytes,
            needs_full_disk_access: fda,
        }
    }

    fn group(id: &str, installed: bool, items: Vec<Item>) -> Group {
        Group {
            id: id.into(),
            name: id.into(),
            icon: "globe",
            detail: "2 profiles".into(),
            app_name: None,
            installed,
            running: false,
            items,
        }
    }

    #[test]
    fn empty_is_only_claimed_when_access_is_known() {
        let g = group("safari", true, vec![]);
        let protected = item("safari:history", DataKind::History, 0, true);
        let open = item("chrome:history", DataKind::History, 0, false);
        assert_eq!(
            row_state(&g, &protected, Some(Access::Granted)),
            RowState::Empty
        );
        assert_eq!(
            row_state(&g, &protected, Some(Access::Missing)),
            RowState::NoAccess
        );
        // No protected folder exists to probe, so nothing can be hidden.
        assert_eq!(
            row_state(&g, &protected, Some(Access::Unknown)),
            RowState::Empty
        );
        assert_eq!(row_state(&g, &open, Some(Access::Missing)), RowState::Empty);
        let sized = item("safari:history", DataKind::History, 10, true);
        assert_eq!(
            row_state(&g, &sized, Some(Access::Granted)),
            RowState::Ready
        );
        assert_eq!(
            row_state(&g, &sized, Some(Access::Missing)),
            RowState::Partial
        );
        assert!(RowState::Partial.selectable() && !RowState::NoAccess.selectable());
        let mut running = g.clone();
        running.running = true;
        assert_eq!(
            row_state(&running, &sized, Some(Access::Granted)),
            RowState::BrowserRunning
        );
    }

    #[test]
    fn row_values_do_not_depend_on_the_browser_running() {
        let protected = |bytes| item("safari:history", DataKind::History, bytes, true);
        let missing = Some(Access::Missing);
        assert_eq!(row_value(&protected(0), Some(Access::Granted)), "Empty");
        assert_eq!(row_value(&protected(0), missing), "No access");
        assert_eq!(
            row_value(&protected(1_500_000), Some(Access::Granted)),
            "1.50 MB"
        );
        assert_eq!(
            row_value(&protected(2_000_000), missing),
            "At least 2.00 MB"
        );
        assert_eq!(row_value(&protected(999), None), "999 bytes");
        // An open browser still shows what was measured, never a bare "0 bytes".
        let mut open = group("safari", true, vec![]);
        open.running = true;
        assert_eq!(
            row_state(&open, &protected(0), missing),
            RowState::BrowserRunning
        );
        assert_eq!(row_value(&protected(0), missing), "No access");
    }

    #[test]
    fn group_description_shows_size_only_when_measured() {
        let mut g = group(
            "chrome",
            true,
            vec![item("chrome:cache", DataKind::Cache, 2_000_000, false)],
        );
        assert_eq!(group_description(&g, None), "2 profiles · 2.00 MB");
        g.items[0].bytes = 0;
        assert_eq!(group_description(&g, None), "2 profiles");
        g.installed = false;
        g.items[0].bytes = 5000;
        assert_eq!(
            group_description(&g, None),
            "App not installed, data left behind \u{b7} 2 profiles \u{b7} 5 KB"
        );
    }

    #[test]
    fn summary_counts_installed_and_leftover_separately() {
        let groups = vec![
            group(
                "safari",
                true,
                vec![item("safari:cache", DataKind::Cache, 1000, false)],
            ),
            group(
                "brave",
                false,
                vec![item("brave:cache", DataKind::Cache, 5000, false)],
            ),
            group(
                "recent",
                true,
                vec![item(
                    "recent:recent-docs",
                    DataKind::RecentDocuments,
                    300,
                    false,
                )],
            ),
        ];
        let selected: HashSet<String> = ["brave:cache".to_string(), "gone:id".to_string()].into();
        let s = summary(&groups, &selected, Some(Access::Granted));
        assert_eq!((s.installed_browsers, s.leftover_browsers), (1, 1));
        assert_eq!((s.browser_bytes, s.recent_bytes), (6000, 300));
        assert_eq!(
            (s.selected_bytes, s.selected_items),
            (5000, 1),
            "unknown ids are not counted"
        );
        assert_eq!(
            s.browser_caption(),
            "1 supported browser installed · 1 with leftover data"
        );
        assert_eq!(s.selected_caption(), "1 item");
        assert_eq!(footer_summary(&s), "1 item selected · 5 KB");
        assert_eq!(clean_button_label(&s), "Clean 5 KB");
        assert_eq!(
            s.clone().scan_stopped(true).browser_caption(),
            "Scan stopped early \u{b7} partial"
        );
        let none = summary(&groups, &HashSet::new(), Some(Access::Granted));
        assert_eq!(footer_summary(&none), "Nothing selected");
        assert_eq!(clean_button_label(&none), "Clean selected");
        assert_eq!(none.selected_caption(), "0 items");

        // Full Disk Access off: protected data is missing from the totals, and the UI says so.
        let mut protected = groups.clone();
        protected[0]
            .items
            .push(item("safari:history", DataKind::History, 0, true));
        let hidden = summary(&protected, &HashSet::new(), Some(Access::Missing));
        assert!(hidden.incomplete);
        assert!(hidden
            .browser_caption()
            .ends_with("protected data not counted"));
        assert!(!summary(&protected, &HashSet::new(), Some(Access::Granted)).incomplete);
        assert_eq!(
            group_description(&protected[0], Some(Access::Missing)),
            "2 profiles \u{b7} 1 KB"
        );
        protected[0].items[0].needs_full_disk_access = true;
        assert_eq!(
            group_description(&protected[0], Some(Access::Missing)),
            "2 profiles \u{b7} at least 1 KB"
        );
    }

    #[test]
    fn trashed_bytes_are_not_reported_as_freed() {
        let mut r = CleanReport {
            cleaned: vec!["a".into(), "b".into()],
            removed: 2,
            ..Default::default()
        };
        r.trashed = 1;
        r.trashed_bytes = 3_000_000_000;
        r.freed = 5_000_000;
        let o = clean_outcome(&r);
        assert_eq!(o.severity, Severity::Success);
        assert_eq!(o.title, "Cleaned 2 items");
        assert_eq!(
            o.message,
            format!(
                "Freed 5.00 MB. Moved 3.00 GB to the {}. The space is freed when you empty it.",
                nera_core::os::TRASH
            )
        );
        r.freed = 0;
        r.trashed_bytes = 0;
        r.trashed = 0;
        assert_eq!(clean_outcome(&r).message, "");
        let stopped = CleanReport {
            cancelled: true,
            ..Default::default()
        };
        let o = clean_outcome(&stopped);
        assert_eq!(
            (o.severity, o.title.as_str()),
            (Severity::Info, "Nothing was cleaned")
        );
    }

    #[test]
    fn running_banner_title_agrees_in_number() {
        assert_eq!(running_title(&["Safari"]), "Safari is open");
        assert_eq!(
            running_title(&["Safari", "Firefox"]),
            "Safari, Firefox are open"
        );
    }

    #[test]
    fn subtitle_mentions_trash_only_for_caches() {
        let trash = nera_core::os::TRASH;
        assert!(row_subtitle(DataKind::Cache, true).ends_with(&format!("Moved to the {trash}")));
        assert!(!row_subtitle(DataKind::Cache, false).contains(trash));
        assert!(!row_subtitle(DataKind::Cookies, true).contains(trash));
    }
}
