//! Pure text for the screen: every string that carries data is built here so
//! the screen, the `dump` example and the unit tests share one implementation.

use crate::engine::{DuplicateFile, DuplicateGroup, ScanReport};
use nera_core::format;
use std::path::Path;
use std::time::SystemTime;

/// A stat tile: label, value, caption.
pub type Tile = (&'static str, String, String);

/// `1234567` -> `1,234,567`.
pub fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Path with the home folder shown as `~` on macOS; in full on Windows
/// (where `~` is not a convention users know).
pub fn tilde(path: &Path) -> String {
    if cfg!(windows) {
        return path.display().to_string();
    }
    let home = nera_core::paths::home();
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Local time of `t`; `None` for a timestamp outside the calendar's range
/// (a corrupt modification date must not crash the screen).
fn local_time(t: SystemTime) -> Option<chrono::DateTime<chrono::Local>> {
    let secs = match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).ok()?,
        Err(e) => i64::try_from(e.duration().as_secs()).ok()?.checked_neg()?,
    };
    chrono::DateTime::from_timestamp(secs, 0).map(|utc| utc.with_timezone(&chrono::Local))
}

/// `5 Nov 2026` in the local time zone.
pub fn date(t: SystemTime) -> String {
    match local_time(t) {
        Some(d) => d.format("%-d %b %Y").to_string(),
        None => "unknown date".to_string(),
    }
}

/// `count(1234, "file")` -> `1,234 files`.
pub fn count(n: usize, noun: &str) -> String {
    format!("{} {noun}{}", thousands(n), if n == 1 { "" } else { "s" })
}

/// A size threshold the way people write it: `100 KB`, `1 MB`, `1.5 MB`.
pub fn threshold(bytes: u64) -> String {
    let s = format::bytes(bytes);
    match s.split_once(' ') {
        Some((num, unit)) if num.contains('.') => {
            format!("{} {unit}", num.trim_end_matches('0').trim_end_matches('.'))
        }
        _ => s,
    }
}

pub fn tile_reclaimable(report: &ScanReport) -> Tile {
    let upper = report.groups.iter().any(|g| g.families() < g.files.len());
    let caption = if upper {
        "At most, keeping one copy of each file"
    } else {
        "Keeping one copy of each file"
    };
    (
        "Reclaimable",
        format::bytes(report.wasted()),
        caption.into(),
    )
}

pub fn tile_sets(report: &ScanReport) -> Tile {
    let largest = report.groups.iter().map(|g| g.size).max().unwrap_or(0);
    (
        "Duplicate sets",
        thousands(report.groups.len()),
        format!("Largest copy {}", format::bytes(largest)),
    )
}

pub fn tile_extra(report: &ScanReport) -> Tile {
    (
        "Extra copies",
        thousands(report.extra_copies()),
        "Copies that take their own space".into(),
    )
}

pub fn tile_compared(report: &ScanReport, min_size: u64) -> Tile {
    (
        "Files checked",
        thousands(report.files_considered),
        format!("{} or larger", threshold(min_size)),
    )
}

/// Message when nothing was found.
pub fn empty_message(report: &ScanReport, min_size: u64) -> String {
    format!(
        "Checked {} of {} or larger in {}.",
        count(report.files_considered, "file"),
        threshold(min_size),
        count(report.folders_searched, "folder")
    )
}

/// Header of one set: `name · 1 MB × 3 copies · 2 MB reclaimable`.
pub fn group_label(group: &DuplicateGroup) -> String {
    let name = group
        .files
        .first()
        .map(DuplicateFile::name)
        .unwrap_or_default();
    let n = group.files.len();
    let copies = if n == 1 {
        "1 copy".to_string()
    } else {
        format!("{n} copies")
    };
    let upper = if group.families() < group.files.len() {
        "up to "
    } else {
        ""
    };
    format!(
        "{name} · {} × {copies} · {upper}{} reclaimable",
        format::bytes(group.size),
        format::bytes(group.wasted())
    )
}

/// Subtitle of one file row: `~/Documents · Modified 5 Nov 2026`.
pub fn file_subtitle(file: &DuplicateFile) -> String {
    let parent = file.path.parent().map(tilde).unwrap_or_default();
    match file.modified {
        Some(m) => format!("{parent} · Modified {}", date(m)),
        None => parent,
    }
}

/// Space a selection frees, in words: `975 KB`, `up to 975 KB`, `nothing`.
fn frees(freed: u64, upper: bool) -> String {
    match (freed, upper) {
        (0, _) => "nothing".to_string(),
        (n, true) => format!("up to {}", format::bytes(n)),
        (n, false) => format::bytes(n),
    }
}

/// `upper`: the size is an upper bound (see `engine::selection_is_upper_bound`).
pub fn footer_summary(selected_count: usize, freed: u64, upper: bool) -> String {
    if selected_count == 0 {
        "No files selected".to_string()
    } else {
        format!(
            "{} selected · frees {}",
            count(selected_count, "file"),
            frees(freed, upper)
        )
    }
}

/// Label of the Trash button: the number of files, never a size (what is
/// freed depends on copies that stay, and is shown next to it).
pub fn trash_button_label(selected_count: usize) -> String {
    if selected_count == 0 {
        concat!("Move to ", nera_core::trash_name!()).to_string()
    } else {
        format!(
            "Move {} to {}",
            count(selected_count, "file"),
            nera_core::os::TRASH
        )
    }
}

pub fn confirm_message(selected_count: usize, freed: u64, upper: bool) -> String {
    format!(
        "{} will be moved to the {trash}, freeing {} once you empty it. One copy of each file is kept, and you can restore items from the {trash}.",
        count(selected_count, "file"),
        frees(freed, upper),
        trash = nera_core::os::TRASH
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Info,
    Success,
    Warning,
    Danger,
}

/// Title and message after moving duplicates to the Trash. Moving to the
/// Trash frees nothing until it is emptied, so this never says "freed" as a
/// done deal.
pub fn trash_result(
    moved: usize,
    failed: usize,
    freed: u64,
    first_failure: &str,
) -> (Kind, String, String) {
    let trash = nera_core::os::TRASH;
    let title = format!("Moved {} to the {trash}", count(moved, "file"));
    let amount = if freed == 0 {
        "No disk space is freed: these copies shared their data with files you kept.".to_string()
    } else {
        format!(
            "Up to {} is freed when you empty the {trash}.",
            format::bytes(freed)
        )
    };
    match (moved, failed) {
        (0, 0) => (
            Kind::Info,
            "Nothing was moved".into(),
            "The operation was stopped.".into(),
        ),
        (0, failed) => (
            Kind::Danger,
            format!("Nothing was moved to the {trash}"),
            format!(
                "{} could not be moved. {first_failure}",
                count(failed, "file")
            ),
        ),
        (_, 0) => (Kind::Success, title, amount),
        (_, failed) => (
            Kind::Warning,
            title,
            format!(
                "{amount} {} could not be moved. {first_failure}",
                count(failed, "file")
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Family;
    use std::path::PathBuf;
    use std::time::Duration;

    fn file(path: &str, family: u64) -> DuplicateFile {
        DuplicateFile {
            path: PathBuf::from(path),
            size: 1_000_000,
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(86_400 * 365)),
            family: Family(1, family),
            allocated: 1_003_520,
            pinned: false,
        }
    }

    #[test]
    fn number_and_path_helpers() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_234_567), "1,234,567");
        #[cfg(target_os = "macos")]
        assert_eq!(
            tilde(&nera_core::paths::home_join("Documents/a")),
            "~/Documents/a"
        );
        #[cfg(windows)]
        assert_eq!(
            tilde(&nera_core::paths::home_join(r"Documents\a")),
            nera_core::paths::home_join(r"Documents\a")
                .display()
                .to_string()
        );
        assert_eq!(tilde(Path::new("/tmp/x")), "/tmp/x");
        assert_eq!(threshold(100_000), "100 KB");
        assert_eq!(threshold(1_000_000), "1 MB");
        assert_eq!(threshold(10_000_000), "10 MB");
        assert_eq!(threshold(100_000_000), "100 MB");
        assert_eq!(threshold(1_500_000), "1.5 MB");
        assert_eq!(threshold(1), "1 byte");
    }

    #[test]
    fn group_and_file_text() {
        let g = DuplicateGroup {
            id: "g".into(),
            size: 1_000_000,
            files: vec![
                file("/tmp/a/x.pdf", 1),
                file("/tmp/b/x.pdf", 2),
                file("/tmp/c/y.pdf", 3),
            ],
        };
        assert_eq!(
            group_label(&g),
            "x.pdf · 1.00 MB × 3 copies · 2.00 MB reclaimable"
        );
        assert_eq!(
            file_subtitle(&g.files[0]),
            format!("/tmp/a · Modified {}", date(g.files[0].modified.unwrap()))
        );
        let two = DuplicateGroup {
            files: g.files[..2].to_vec(),
            ..g.clone()
        };
        assert_eq!(
            group_label(&two),
            "x.pdf · 1.00 MB × 2 copies · 1.00 MB reclaimable"
        );
        // Hard links / clones count once.
        let shared = DuplicateGroup {
            files: vec![file("/a/x", 1), file("/b/x", 1), file("/c/x", 2)],
            ..g
        };
        assert_eq!(
            group_label(&shared),
            "x · 1.00 MB × 3 copies · up to 1.00 MB reclaimable"
        );
    }

    #[test]
    fn footer_and_results_never_claim_space_is_already_free() {
        assert_eq!(footer_summary(0, 0, false), "No files selected");
        assert_eq!(
            footer_summary(1, 1_000_000, false),
            "1 file selected · frees 1.00 MB"
        );
        assert_eq!(
            footer_summary(3, 2_500_000, true),
            "3 files selected · frees up to 2.50 MB"
        );
        assert_eq!(
            footer_summary(1, 0, false),
            "1 file selected · frees nothing"
        );
        let trash = nera_core::os::TRASH;
        assert_eq!(trash_button_label(0), format!("Move to {trash}"));
        assert_eq!(trash_button_label(1), format!("Move 1 file to {trash}"));
        assert_eq!(
            trash_button_label(2_345),
            format!("Move 2,345 files to {trash}")
        );
        assert!(confirm_message(1, 0, false).contains("freeing nothing once"));
        assert!(
            confirm_message(2, 2_000_000, true).contains("freeing up to 2.00 MB once you empty it")
        );
        let (kind, title, msg) = trash_result(2, 0, 2_000_000, "");
        assert_eq!(
            (kind, title),
            (Kind::Success, format!("Moved 2 files to the {trash}"))
        );
        assert_eq!(
            msg,
            format!("Up to 2.00 MB is freed when you empty the {trash}.")
        );
        assert!(!msg.starts_with("Freed"));
        let (kind, _, msg) = trash_result(1, 0, 0, "");
        assert_eq!(kind, Kind::Success);
        assert!(msg.starts_with("No disk space is freed"));
        let (kind, _, msg) = trash_result(1, 2, 1_000_000, "Changed since the scan, kept.");
        assert_eq!(kind, Kind::Warning);
        assert!(msg.contains("2 files could not be moved. Changed"));
        assert_eq!(trash_result(0, 0, 0, "").0, Kind::Info);
        assert_eq!(trash_result(0, 1, 0, "x").0, Kind::Danger);
    }
}
