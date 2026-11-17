//! Pure text for the screen: every string that carries data is built here so
//! the screen, the `dump` example and the unit tests share one implementation.

use crate::engine::{self, JunkFolder, Project, ScanOutcome};
use nera_core::format;
use std::time::SystemTime;

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

/// `count(1234, "folder")` -> `1,234 folders`.
pub fn count(n: usize, noun: &str) -> String {
    format!("{} {noun}{}", thousands(n), if n == 1 { "" } else { "s" })
}

/// A stat tile: label, value, caption.
pub type Tile = (&'static str, String, String);

/// `at least 4.8 MB` wording shared by tiles, footer and notices.
fn frees_at_least(unique: u64) -> String {
    format!("frees at least {}", format::bytes(unique))
}

pub fn tile_size(outcome: &ScanOutcome) -> Tile {
    let (size, unique) = (outcome.total_size(), outcome.total_unique());
    let mut caption = format!("in {}", count(outcome.junk_count(), "folder"));
    if engine::is_shared(size, unique) {
        caption = format!("{caption} · {}", frees_at_least(unique));
    }
    ("Size on disk", format::bytes(size), caption)
}

pub fn tile_projects(outcome: &ScanOutcome) -> Tile {
    (
        "Projects with junk",
        thousands(outcome.projects.len()),
        format!("{} searched", count(outcome.folders_scanned, "folder")),
    )
}

pub fn tile_selected(selected_size: u64, selected_unique: u64, selected_count: usize) -> Tile {
    let caption = if selected_count == 0 {
        "Nothing selected".to_string()
    } else if engine::is_shared(selected_size, selected_unique) {
        format!(
            "{} · {}",
            count(selected_count, "folder"),
            frees_at_least(selected_unique)
        )
    } else {
        count(selected_count, "folder")
    };
    ("Selected", format::bytes(selected_size), caption)
}

/// Message when the scan finished without finding anything.
pub fn empty_message(outcome: &ScanOutcome) -> String {
    format!(
        "Searched {} in {:.1} seconds. Nothing regenerable is taking up space.",
        count(outcome.folders_scanned, "folder"),
        outcome.elapsed.as_secs_f64()
    )
}

/// Subtitle of a junk folder row: `~/path · modified 3 days ago`.
pub fn junk_subtitle(junk: &JunkFolder, now: SystemTime) -> String {
    let mut out = engine::display_path(&junk.path);
    if let Some(m) = junk.modified {
        out = format!("{out} · modified {}", engine::age_label(m, now));
    }
    if junk.is_shared() {
        out = format!(
            "{out} · shares data with other files, {}",
            frees_at_least(junk.unique)
        );
    }
    out
}

/// Subtitle of a project row: `~/path · last changed 2 weeks ago · 3 folders, 1 selected`.
pub fn project_subtitle(project: &Project, selected: usize, now: SystemTime) -> String {
    let mut sub = engine::display_path(&project.root);
    if let Some(t) = project.last_activity {
        sub = format!("{sub} · last changed {}", engine::age_label(t, now));
    }
    sub = format!("{sub} · {}", count(project.junk.len(), "folder"));
    if selected > 0 {
        sub = format!("{sub}, {selected} selected");
    }
    sub
}

/// Badges of a project row: up to two ecosystems, then `Inactive`.
pub fn project_badges(project: &Project, now: SystemTime) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = project
        .ecosystems
        .iter()
        .take(2)
        .map(|e| e.label())
        .collect();
    if engine::is_inactive(project.last_activity, now) {
        out.push("Inactive");
    }
    out
}

/// Footer summary line.
pub fn footer_summary(selected_count: usize, selected_size: u64, selected_unique: u64) -> String {
    if selected_count == 0 {
        return "No folders selected".to_string();
    }
    let mut out = format!(
        "{} selected · {}",
        count(selected_count, "folder"),
        format::bytes(selected_size)
    );
    if engine::is_shared(selected_size, selected_unique) {
        out = format!("{out} · {}", frees_at_least(selected_unique));
    }
    out
}

/// Extra sentence for the confirmation when part of the data is shared.
pub fn shared_note(size: u64, unique: u64) -> String {
    // On Windows only hard links are detected (there are no APFS clones).
    let how = if cfg!(windows) {
        "hard links"
    } else {
        "clones"
    };
    if engine::is_shared(size, unique) {
        format!(
            " Part of it is shared with other files ({how}), so this frees at least {}.",
            format::bytes(unique)
        )
    } else {
        String::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Info,
    Success,
    Warning,
    Danger,
}

/// Title and message of the toast / banner shown after a cleanup. `size` is
/// what was removed, `unique` the part of it that is guaranteed free. Moving
/// to the Trash frees nothing until the Trash is emptied, so that case never
/// says "freed".
pub fn clean_result(
    removed: usize,
    failed: usize,
    size: u64,
    unique: u64,
    to_trash: bool,
    first_failure: &str,
) -> (Kind, String, String) {
    let verb = if to_trash { "Moved" } else { "Deleted" };
    let dest = if to_trash {
        concat!(" to the ", nera_core::trash_name!())
    } else {
        ""
    };
    let amount = if to_trash {
        format!(
            "{} moved to the {}. Space is freed when you empty it.",
            format::bytes(size),
            nera_core::os::TRASH
        )
    } else if engine::is_shared(size, unique) {
        format!(
            "Removed {}. At least {} of disk space is freed; the rest is shared with files kept elsewhere.",
            format::bytes(size),
            format::bytes(unique)
        )
    } else {
        format!("Freed {}.", format::bytes(size))
    };
    match (removed, failed) {
        (0, 0) => (
            Kind::Info,
            "Nothing was removed".into(),
            "The cleanup was stopped.".into(),
        ),
        (0, failed) => (
            Kind::Danger,
            "Nothing was removed".into(),
            format!(
                "{} could not be removed. {first_failure}",
                count(failed, "folder")
            ),
        ),
        (n, 0) => (
            Kind::Success,
            format!("{verb} {}{dest}", count(n, "folder")),
            amount,
        ),
        (n, failed) => (
            Kind::Warning,
            format!("{verb} {}{dest}", count(n, "folder")),
            format!(
                "{amount} {} could not be removed. {first_failure}",
                count(failed, "folder")
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn junk(size: u64, unique: u64) -> JunkFolder {
        JunkFolder {
            path: nera_core::paths::home_join("p/node_modules"),
            relative: "node_modules".into(),
            ecosystem: engine::Ecosystem::Node,
            size,
            unique,
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(86_400 * 10)),
        }
    }

    fn project(junk: Vec<JunkFolder>) -> Project {
        Project {
            root: PathBuf::from("/x/p"),
            name: "p".into(),
            ecosystems: vec![
                engine::Ecosystem::NextJs,
                engine::Ecosystem::Node,
                engine::Ecosystem::Python,
            ],
            last_activity: Some(SystemTime::UNIX_EPOCH),
            junk,
        }
    }

    #[test]
    fn counts_have_thousands_separators() {
        assert_eq!(count(1, "folder"), "1 folder");
        assert_eq!(count(0, "folder"), "0 folders");
        assert_eq!(count(59_089, "folder"), "59,089 folders");
        assert_eq!(thousands(1_000), "1,000");
    }

    #[test]
    fn subtitles_and_badges() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(86_400 * 15);
        // `~`-abbreviated on macOS, in full on Windows.
        let shown = engine::display_path(&junk(1, 1).path);
        #[cfg(target_os = "macos")]
        assert_eq!(shown, "~/p/node_modules");
        #[cfg(windows)]
        assert_eq!(
            shown,
            nera_core::paths::home_join("p/node_modules")
                .display()
                .to_string()
        );
        let root = engine::display_path(&PathBuf::from("/x/p"));
        assert_eq!(
            junk_subtitle(&junk(5_000_000, 5_000_000), now),
            format!("{shown} · modified 5 days ago")
        );
        assert_eq!(
            junk_subtitle(&junk(5_000_000, 1_000_000), now),
            format!("{shown} · modified 5 days ago · shares data with other files, frees at least 1.00 MB")
        );
        let p = project(vec![junk(1, 1), junk(1, 1)]);
        assert_eq!(
            project_subtitle(&p, 0, now),
            format!("{root} · last changed 2 weeks ago · 2 folders")
        );
        assert_eq!(
            project_subtitle(&p, 1, now),
            format!("{root} · last changed 2 weeks ago · 2 folders, 1 selected")
        );
        assert_eq!(
            project_subtitle(&project(vec![junk(1, 1)]), 0, now),
            format!("{root} · last changed 2 weeks ago · 1 folder")
        );
        assert_eq!(project_badges(&p, now), vec!["Next.js", "Node.js"]);
        let later = SystemTime::UNIX_EPOCH + Duration::from_secs(86_400 * 91);
        assert_eq!(
            project_badges(&p, later),
            vec!["Next.js", "Node.js", "Inactive"]
        );
    }

    #[test]
    fn selection_and_result_text_do_not_overclaim() {
        assert_eq!(footer_summary(0, 0, 0), "No folders selected");
        assert_eq!(
            footer_summary(1, 2_000_000_000, 2_000_000_000),
            "1 folder selected · 2.00 GB"
        );
        assert_eq!(
            footer_summary(3, 2_000_000_000, 5_000_000),
            "3 folders selected · 2.00 GB · frees at least 5.00 MB"
        );
        assert_eq!(tile_selected(0, 0, 0).2, "Nothing selected");
        assert_eq!(tile_selected(10_000_000, 10_000_000, 2).2, "2 folders");
        let (kind, title, msg) = clean_result(2, 0, 3_000_000, 3_000_000, true, "");
        let trash = nera_core::os::TRASH;
        assert_eq!(
            (kind, title),
            (Kind::Success, format!("Moved 2 folders to the {trash}"))
        );
        assert_eq!(
            msg,
            format!("3.00 MB moved to the {trash}. Space is freed when you empty it.")
        );
        let (_, title, msg) = clean_result(1, 0, 3_000_000, 3_000_000, false, "");
        assert_eq!(
            (title.as_str(), msg.as_str()),
            ("Deleted 1 folder", "Freed 3.00 MB.")
        );
        let (_, _, msg) = clean_result(1, 0, 3_000_000, 1_000_000, false, "");
        assert!(msg.starts_with("Removed 3.00 MB. At least 1.00 MB of disk space is freed"));
        let (kind, _, msg) = clean_result(1, 2, 3_000_000, 3_000_000, false, "a: denied");
        assert_eq!(kind, Kind::Warning);
        assert!(msg.ends_with("2 folders could not be removed. a: denied"));
        assert_eq!(clean_result(0, 0, 0, 0, false, "").0, Kind::Info);
        assert_eq!(clean_result(0, 3, 0, 0, false, "x").0, Kind::Danger);
        assert!(shared_note(10_000_000, 10_000_000).is_empty());
        assert!(shared_note(10_000_000, 1_000_000).contains("at least 1.00 MB"));
    }
}
