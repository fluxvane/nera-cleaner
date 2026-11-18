//! Every number and sentence the Dashboard and System Junk screens show, as
//! pure functions of the scan state. The screens only lay these strings out;
//! `examples/dump.rs` prints them, so what is verified is what is displayed.

use crate::engine::{self, Disposal, LastCleanup, Scanned};
use crate::store::{categories, Phase, ScanResults};
use nera_core::disk::DiskSpace;
use nera_core::format;
use nera_core::os::TRASH;
use nera_core::paths::standardize;

/// File and disk sizes as the platform's file manager shows them: decimal
/// units like Finder on macOS; on Windows binary units labelled KB/MB/GB,
/// formatted by Windows itself as File Explorer does.
#[cfg(target_os = "macos")]
pub fn bytes(n: u64) -> String {
    format::bytes(n)
}
#[cfg(windows)]
pub fn bytes(n: u64) -> String {
    crate::engine::win::explorer_bytes(n)
}

/// Finder says "X available of Y"; File Explorer's This PC says "X free of Y".
#[cfg(target_os = "macos")]
const FREE_OF: &str = "available of";
#[cfg(windows)]
const FREE_OF: &str = "free of";

/// Badge of categories that always go to the Trash (Recycle Bin).
pub const MOVES_TO_TRASH: &str = concat!("Moves to ", nera_core::trash_name!());

/// Text of one stat tile.
#[derive(Clone, Debug, PartialEq)]
pub struct Tile {
    pub title: &'static str,
    pub value: String,
    pub caption: String,
    /// Meter fill, when the tile has one.
    pub fraction: Option<f64>,
}

/// Dashboard "Disk used" tile. `used` is the capacity minus the free space
/// `statfs` reports, which is what `df`, `diskutil` and `system_profiler` call
/// free. Purgeable space that Finder adds to "Available" is not counted as free.
pub fn disk_tile(disk: Option<DiskSpace>) -> Tile {
    match disk {
        Some(d) => Tile {
            title: "Disk used",
            value: bytes(d.used()),
            caption: format!(
                "{} {FREE_OF} {} · {}",
                bytes(d.free),
                bytes(d.total),
                d.status().label()
            ),
            fraction: Some(d.used_fraction()),
        },
        None => Tile {
            title: "Disk used",
            value: "Unavailable".into(),
            caption: "The startup disk could not be read".into(),
            fraction: None,
        },
    }
}

/// Dashboard "Junk found" tile.
pub fn junk_tile(results: Option<&ScanResults>, phase: Phase) -> Tile {
    let tile = |value: String, caption: String| Tile {
        title: "Junk found",
        value,
        caption,
        fraction: None,
    };
    match (results, phase) {
        (_, Phase::Scanning) => tile("Scanning".into(), "This can take a minute".into()),
        // The old sizes are out of date as soon as items start to go.
        (_, Phase::Cleaning) => tile("Cleaning".into(), "Sizes update when it finishes".into()),
        (Some(r), Phase::Idle) => tile(bytes(r.total()), junk_caption(r)),
        (None, Phase::Idle) => tile("Not scanned".into(), "Run Smart Scan to find out".into()),
    }
}

/// "3 categories · 1.2 GB safe to remove · 4 GB needs review".
pub fn junk_caption(r: &ScanResults) -> String {
    let review = r.total().saturating_sub(r.safe_total());
    let mut s = format!(
        "{} · {} safe to remove",
        categories(r.non_empty()),
        bytes(r.safe_total())
    );
    if review > 0 {
        s.push_str(&format!(" · {} needs review", bytes(review)));
    }
    s
}

/// Dashboard "Last cleanup" tile. `now` is seconds since the epoch.
pub fn last_cleanup_tile(last: Option<LastCleanup>, now: u64) -> Tile {
    match last {
        Some(l) => Tile {
            title: "Last cleanup",
            value: bytes(l.bytes),
            caption: format!(
                "{} · {}{}",
                categories(l.categories),
                engine::relative_time(l.at, now),
                match l.trashed {
                    0 => String::new(),
                    t if t >= l.bytes => format!(" · in the {TRASH}"),
                    t => format!(" · {} in the {TRASH}", bytes(t)),
                }
            ),
            fraction: None,
        },
        None => Tile {
            title: "Last cleanup",
            value: "None yet".into(),
            caption: "Cleaned space shows up here".into(),
            fraction: None,
        },
    }
}

/// "Last scan" section of the Dashboard.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    /// "2 minutes ago, in 18s".
    pub description: String,
    /// `None` when nothing was found.
    pub body: Option<SummaryBody>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SummaryBody {
    pub headline: String,
    pub detail: String,
    /// Five biggest categories: (name, size).
    pub top: Vec<(&'static str, String)>,
}

pub fn dashboard_summary(r: &ScanResults, now: u64) -> Summary {
    let description = format!(
        "{}, in {}",
        engine::relative_time(r.scanned_at, now),
        format::duration_secs(r.elapsed.as_secs())
    );
    if r.total() == 0 {
        return Summary {
            description,
            body: None,
        };
    }
    let mut top: Vec<&Scanned> = r.categories.iter().filter(|c| c.size > 0).collect();
    top.sort_by(|a, b| {
        b.size
            .cmp(&a.size)
            .then(a.category.name.cmp(b.category.name))
    });
    let review = r.total().saturating_sub(r.safe_total());
    let mut detail = format!(
        "Found in {}. {} is safe to remove",
        categories(r.non_empty()),
        bytes(r.safe_total())
    );
    if review > 0 {
        detail.push_str(&format!(" and {} needs review", bytes(review)));
    }
    detail.push('.');
    Summary {
        description,
        body: Some(SummaryBody {
            headline: format!("{} of junk found", bytes(r.total())),
            detail,
            top: top
                .into_iter()
                .take(5)
                .map(|c| (c.category.name, bytes(c.size)))
                .collect(),
        }),
    }
}

/// Line above the System Junk list.
pub fn toolbar_summary(r: &ScanResults, now: u64) -> String {
    format!(
        "{} found in {} · scanned {}",
        bytes(r.total()),
        categories(r.non_empty()),
        engine::relative_time(r.scanned_at, now).to_lowercase()
    )
}

/// One row of the System Junk list.
#[derive(Clone, Debug, PartialEq)]
pub struct RowText {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub value: String,
    pub badges: Vec<&'static str>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GroupText {
    pub label: &'static str,
    /// Sum of the rows of this group.
    pub total: String,
    pub rows: Vec<RowText>,
}

pub fn groups(found: &[Scanned]) -> Vec<GroupText> {
    engine::grouped(found)
        .into_iter()
        .map(|(group, items)| GroupText {
            label: group.label(),
            total: bytes(items.iter().map(|s| s.size).sum()),
            rows: items
                .into_iter()
                .map(|s| {
                    let mut badges = Vec::new();
                    if !s.category.safe {
                        badges.push("Review first");
                    }
                    if s.category.always_trashes() {
                        badges.push(MOVES_TO_TRASH);
                    }
                    RowText {
                        id: s.category.id,
                        name: s.category.name,
                        description: s.category.description,
                        value: bytes(s.size),
                        badges,
                    }
                })
                .collect(),
        })
        .collect()
}

/// Note under the list for categories with nothing in them.
pub fn empty_note(found: &[Scanned]) -> Option<String> {
    let empty = engine::empty_names(found);
    (!empty.is_empty()).then(|| {
        format!(
            "Nothing to clean in {}: {}.",
            categories(empty.len()),
            empty.join(", ")
        )
    })
}

/// Note under the list for categories that do hold data, owned by the system,
/// that Nera cannot remove without an administrator.
pub fn blocked_note(found: &[Scanned]) -> Option<String> {
    let blocked = engine::blocked_categories(found);
    (!blocked.is_empty()).then(|| {
        let list: Vec<String> = blocked
            .iter()
            .map(|(name, bytes)| format!("{name} ({})", self::bytes(*bytes)))
            .collect();
        format!("{BLOCKED_PREFIX} {}.", list.join(", "))
    })
}

#[cfg(target_os = "macos")]
const BLOCKED_PREFIX: &str = "Owned by the system, so Nera cannot remove it:";
#[cfg(windows)]
const BLOCKED_PREFIX: &str = "Needs an administrator to remove, so it is not counted:";

/// Names of categories with nothing counted because a target folder could
/// not be read (on Windows: system folders that need an administrator).
pub fn unreadable_categories(r: &ScanResults) -> Vec<&'static str> {
    let denied: Vec<std::path::PathBuf> = r.denied.iter().map(standardize).collect();
    let mut names: Vec<&'static str> = r
        .categories
        .iter()
        .filter(|s| {
            s.size == 0
                && s.category
                    .paths
                    .iter()
                    .any(|p| denied.contains(&standardize(p)))
        })
        .map(|s| s.category.name)
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Note for categories whose folders could not be read.
pub fn unreadable_note(r: &ScanResults) -> Option<String> {
    let names = unreadable_categories(r);
    (!names.is_empty()).then(|| {
        format!(
            "Needs an administrator to read, so it is not counted: {}. Run Nera as administrator to include it.",
            names.join(", ")
        )
    })
}

/// Notes under the System Junk list, in display order. On macOS folders that
/// cannot be read are explained by the Full Disk Access banner instead.
pub fn notes(r: &ScanResults) -> Vec<String> {
    #[cfg(target_os = "macos")]
    let notes = [blocked_note(&r.categories), empty_note(&r.categories)];
    #[cfg(windows)]
    let notes = {
        let unreadable = unreadable_categories(r);
        let readable: Vec<Scanned> = r
            .categories
            .iter()
            .filter(|s| !unreadable.contains(&s.category.name))
            .cloned()
            .collect();
        [
            unreadable_note(r),
            blocked_note(&readable),
            empty_note(&readable),
        ]
    };
    notes.into_iter().flatten().collect()
}

/// The Trash is offered for emptying after a cleanup from this size.
pub const EMPTY_TRASH_MIN: u64 = 1_000_000;
/// Cloud drives with less than this on disk are not worth a row.
pub const CLOUD_DRIVE_MIN: u64 = 50_000_000;
pub const CLOUD_HINT: &str = "These are your files, not junk, and Nera never removes them. To free the space, select files or folders in Finder, right-click and choose Remove Download. They stay in the cloud and download again when opened.";

/// System feature data below this is not worth a row.
pub const SYSTEM_ASSET_MIN: u64 = 100_000_000;
pub const SYSTEM_ASSET_HINT: &str = "macOS downloads and owns this data, so Nera cannot remove it, and macOS downloads it again while the feature is on. If you don't use a feature, turn it off in System Settings: macOS then removes what it no longer needs, which can take a while.";

/// "me@example.com · 1,204 files on this Mac".
pub fn cloud_subtitle(drive: &crate::engine::CloudDrive) -> String {
    let files = format!(
        "{} on this Mac",
        nera_core::format::count(drive.local_files as usize, "file")
    );
    match &drive.account {
        Some(account) => format!("{account} · {files}"),
        None => files,
    }
}

/// What the disk reports before and after a cleanup. The gain is stated
/// only when there is one: other apps write to the disk meanwhile, and
/// items moved to the Trash free nothing yet.
pub fn measured_line(before: u64, after: u64) -> String {
    let gain = after.saturating_sub(before);
    if gain >= 1_000_000 {
        format!(
            "Free space on the disk went from {} to {} (+{}).",
            bytes(before),
            bytes(after),
            bytes(gain)
        )
    } else {
        format!(
            "Free space on the disk is {}, about the same as before.",
            bytes(after)
        )
    }
}

/// Text of the System Junk footer.
#[derive(Clone, Debug, PartialEq)]
pub struct FooterText {
    pub headline: String,
    pub hint: String,
    pub button: String,
}

pub fn footer(count: usize, bytes: u64, disposal: &Disposal) -> FooterText {
    if count == 0 {
        FooterText {
            headline: "Nothing selected".into(),
            hint: "Choose what to clean".into(),
            button: "Clean".into(),
        }
    } else {
        FooterText {
            headline: format!("{} selected · {}", count, self::bytes(bytes)),
            hint: disposal.short().into(),
            button: format!("Clean {}", self::bytes(bytes)),
        }
    }
}

/// Confirmation dialog: (title, message).
pub fn confirm_text(
    count: usize,
    bytes: u64,
    disposal: &Disposal,
    review: &[&str],
) -> (String, String) {
    let mut message = format!(
        "{} ({}) will be cleaned. {}",
        categories(count),
        self::bytes(bytes),
        disposal.sentence()
    );
    if !review.is_empty() {
        message.push_str(&format!(
            " Includes items that need review: {}.",
            review.join(", ")
        ));
    }
    (format!("Clean {}?", self::bytes(bytes)), message)
}

/// Count and bytes of the categories in `selected` that have something to remove.
pub fn selection_totals(
    found: &[Scanned],
    selected: &std::collections::HashSet<&'static str>,
) -> (usize, u64) {
    let picked = found
        .iter()
        .filter(|s| s.size > 0 && selected.contains(s.category.id));
    picked.fold((0, 0), |(n, b), s| (n + 1, b + s.size))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::engine::{Category, Group, Mode};
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::time::Duration;

    fn scanned(id: &'static str, group: Group, size: u64, safe: bool) -> Scanned {
        Scanned {
            category: Category {
                id,
                name: id,
                description: "d",
                icon: "folder",
                group,
                paths: vec![PathBuf::from("/x")],
                safe,
                mode: Mode::Contents,
            },
            size,
            blocked: 0,
        }
    }

    fn results(categories: Vec<Scanned>) -> ScanResults {
        ScanResults {
            categories,
            denied: vec![],
            full_disk_access: None,
            scanned_at: 1000,
            elapsed: Duration::from_secs(75),
        }
    }

    #[test]
    fn disk_tile_uses_capacity_minus_finder_available() {
        let t = disk_tile(Some(DiskSpace {
            total: 994_662_584_320,
            free: 27_762_323_456,
        }));
        assert_eq!(t.value, "967 GB");
        assert_eq!(t.caption, "27.8 GB available of 995 GB · Almost full");
        assert!((t.fraction.unwrap() - 0.9721).abs() < 1e-3);
        assert_eq!(disk_tile(None).value, "Unavailable");
    }

    #[test]
    fn junk_tile_separates_safe_from_review() {
        let r = results(vec![
            scanned("a", Group::Caches, 3_000_000_000, true),
            scanned("b", Group::AiVm, 12_000_000_000, false),
            scanned("c", Group::Logs, 0, true),
        ]);
        let t = junk_tile(Some(&r), Phase::Idle);
        assert_eq!(t.value, "15.0 GB");
        assert_eq!(
            t.caption,
            "2 categories · 3.00 GB safe to remove · 12.0 GB needs review"
        );
        assert_eq!(junk_tile(Some(&r), Phase::Scanning).value, "Scanning");
        assert_eq!(junk_tile(Some(&r), Phase::Cleaning).value, "Cleaning");
        assert_eq!(junk_tile(None, Phase::Idle).value, "Not scanned");
        let all_safe = results(vec![scanned("a", Group::Caches, 1_500_000, true)]);
        assert_eq!(
            junk_tile(Some(&all_safe), Phase::Idle).caption,
            "1 category · 1.50 MB safe to remove"
        );
    }

    #[test]
    fn last_cleanup_mentions_the_trash() {
        let l = LastCleanup {
            bytes: 2_000_000_000,
            categories: 3,
            at: 0,
            trashed: 2_000_000_000,
        };
        assert_eq!(
            last_cleanup_tile(Some(l), 7200).caption,
            "3 categories · 2 hours ago · in the Trash"
        );
        let l = LastCleanup { trashed: 0, ..l };
        assert_eq!(
            last_cleanup_tile(Some(l), 90).caption,
            "3 categories · 1 minute ago"
        );
        let l = LastCleanup {
            trashed: 500_000_000,
            ..l
        };
        assert_eq!(
            last_cleanup_tile(Some(l), 90).caption,
            "3 categories · 1 minute ago · 500 MB in the Trash"
        );
    }

    #[test]
    fn summary_lists_top_five_and_scan_age() {
        let found: Vec<Scanned> = ["a", "b", "c", "d", "e", "f"]
            .iter()
            .enumerate()
            .map(|(i, id)| scanned(id, Group::Caches, (i as u64 + 1) * 2_000_000, true))
            .collect();
        let r = results(found);
        let s = dashboard_summary(&r, 1130);
        assert_eq!(s.description, "2 minutes ago, in 1m 15s");
        let body = s.body.unwrap();
        assert_eq!(body.headline, "42.0 MB of junk found");
        assert_eq!(body.top.len(), 5);
        assert_eq!(body.top[0], ("f", "12.0 MB".to_string()));
        assert_eq!(
            body.detail,
            "Found in 6 categories. 42.0 MB is safe to remove."
        );
        assert!(
            dashboard_summary(&results(vec![scanned("a", Group::Caches, 0, true)]), 1000)
                .body
                .is_none()
        );
    }

    #[test]
    fn selection_footer_and_confirmation_agree() {
        let found = vec![
            scanned("a", Group::Caches, 5_000_000, true),
            scanned("b", Group::Caches, 0, true),
        ];
        let sel: HashSet<&'static str> = ["a", "b"].into_iter().collect();
        let (n, bytes) = selection_totals(&found, &sel);
        assert_eq!((n, bytes), (1, 5_000_000));
        let d = Disposal::of(&[&found[0].category], true);
        let f = footer(n, bytes, &d);
        assert_eq!(f.headline, "1 selected · 5.00 MB");
        assert_eq!(f.button, "Clean 5.00 MB");
        let (title, msg) = confirm_text(n, bytes, &d, &[]);
        assert_eq!(title, "Clean 5.00 MB?");
        assert!(msg.starts_with("1 category (5.00 MB) will be cleaned."));
        assert_eq!(
            footer(0, 0, &Disposal::default()).headline,
            "Nothing selected"
        );
    }

    #[test]
    fn group_totals_equal_row_sums() {
        let found = vec![
            scanned("a", Group::Caches, 1_000_000, true),
            scanned("b", Group::Caches, 2_000_000, false),
            scanned("z", Group::Logs, 0, true),
        ];
        let g = groups(&found);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].total, "3.00 MB");
        assert_eq!(g[0].rows[0].id, "b");
        assert_eq!(g[0].rows[0].badges, vec!["Review first"]);
        assert_eq!(
            empty_note(&found).unwrap(),
            "Nothing to clean in 1 category: z."
        );
        assert_eq!(blocked_note(&found), None);
    }

    #[test]
    fn system_owned_data_is_not_called_empty() {
        let mut locked = scanned("logs", Group::Logs, 0, true);
        locked.blocked = 270_000_000;
        let found = vec![locked, scanned("z", Group::Logs, 0, true)];
        assert_eq!(
            empty_note(&found).unwrap(),
            "Nothing to clean in 1 category: z."
        );
        assert_eq!(
            blocked_note(&found).unwrap(),
            "Owned by the system, so Nera cannot remove it: logs (270 MB)."
        );
    }
}

/// The same checks as `tests`, with sizes formatted the way Windows does.
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use crate::engine::{Category, Group, Mode};
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::time::Duration;

    fn scanned(id: &'static str, group: Group, size: u64, safe: bool) -> Scanned {
        Scanned {
            category: Category {
                id,
                name: id,
                description: "d",
                icon: "folder",
                group,
                paths: vec![PathBuf::from(format!(r"C:\x\{id}"))],
                safe,
                mode: Mode::Contents,
            },
            size,
            blocked: 0,
        }
    }

    fn results(categories: Vec<Scanned>) -> ScanResults {
        ScanResults {
            categories,
            denied: vec![],
            full_disk_access: None,
            scanned_at: 1000,
            elapsed: Duration::from_secs(75),
        }
    }

    #[test]
    fn disk_tile_says_free_like_explorer() {
        let gib = 1024 * 1024 * 1024;
        let d = DiskSpace {
            total: 476 * gib,
            free: 100 * gib,
        };
        let t = disk_tile(Some(d));
        assert_eq!(t.value, bytes(376 * gib));
        assert_eq!(
            t.caption,
            format!(
                "{} free of {} · Filling up",
                bytes(100 * gib),
                bytes(476 * gib)
            )
        );
        assert!(t.value.contains("376"));
        assert_eq!(disk_tile(None).value, "Unavailable");
    }

    #[test]
    fn tiles_summary_and_footer_use_windows_words() {
        let r = results(vec![
            scanned("a", Group::Caches, 3_000_000_000, true),
            scanned("b", Group::AiVm, 12_000_000_000, false),
            scanned("c", Group::Logs, 0, true),
        ]);
        assert_eq!(
            junk_tile(Some(&r), Phase::Idle).caption,
            format!(
                "2 categories · {} safe to remove · {} needs review",
                bytes(3_000_000_000),
                bytes(12_000_000_000)
            )
        );
        let l = LastCleanup {
            bytes: 2_000_000,
            categories: 3,
            at: 0,
            trashed: 2_000_000,
        };
        assert_eq!(
            last_cleanup_tile(Some(l), 7200).caption,
            "3 categories · 2 hours ago · in the Recycle Bin"
        );
        let found = vec![scanned("a", Group::Caches, 5_000_000, true)];
        let sel: HashSet<&'static str> = ["a"].into_iter().collect();
        let (n, b) = selection_totals(&found, &sel);
        let d = Disposal::of(&[&found[0].category], true);
        assert_eq!(footer(n, b, &d).hint, "Moves to the Recycle Bin");
        assert_eq!(
            footer(n, b, &d).button,
            format!("Clean {}", bytes(5_000_000))
        );
        assert!(confirm_text(n, b, &d, &[]).1.contains("Recycle Bin"));
        assert_eq!(MOVES_TO_TRASH, "Moves to Recycle Bin");
    }

    #[test]
    fn unreadable_system_folders_are_not_called_empty() {
        let mut r = results(vec![
            scanned("Windows temporary files", Group::Caches, 0, true),
            scanned("Crash dumps", Group::Logs, 0, true),
            scanned("Temporary files", Group::Caches, 1_000, true),
        ]);
        let mut locked = scanned("System error reports", Group::Logs, 0, true);
        locked.blocked = 4096;
        r.categories.push(locked);
        r.denied = vec![PathBuf::from(r"c:\x\Windows temporary files")];
        assert_eq!(unreadable_categories(&r), vec!["Windows temporary files"]);
        assert_eq!(
            notes(&r),
            vec![
                "Needs an administrator to read, so it is not counted: Windows temporary files. Run Nera as administrator to include it.".to_string(),
                format!(
                    "Needs an administrator to remove, so it is not counted: System error reports ({}).",
                    bytes(4096)
                ),
                "Nothing to clean in 1 category: Crash dumps.".to_string(),
            ]
        );
        // Windows has no Full Disk Access: no banner, the notes explain.
        assert!(!r.needs_full_disk_access());
    }
}

#[cfg(test)]
mod measured_tests {
    use super::{bytes, measured_line};

    #[test]
    fn states_a_gain_only_when_there_is_one() {
        // Compared through `bytes`: macOS shows decimal units, Windows binary ones.
        let gb = 1_000_000_000u64;
        let line = measured_line(60 * gb, 69 * gb);
        assert!(
            line.contains(&bytes(60 * gb))
                && line.contains(&bytes(69 * gb))
                && line.contains(&format!("(+{})", bytes(9 * gb))),
            "{line}"
        );
        // Moved to the Trash, or the disk lost space meanwhile: no gain claimed.
        assert!(!measured_line(60 * gb, 60 * gb).contains('+'));
        assert!(!measured_line(60 * gb, 59 * gb).contains('+'));
    }
}
