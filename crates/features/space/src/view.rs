//! Pure text shown by the Space Lens screen. Every number and label the
//! screen displays is produced here so it can be unit-tested and printed by
//! `examples/dump.rs` exactly as the UI would show it.

use crate::engine::{self, Entry, ItemKind, LargeFile, Listing};
use nera_core::disk::DiskSpace;
use nera_core::format;

/// Bytes in decimal units like Finder ("1.50 MB", "20.4 GB"), rounded first
/// and then given the digits of the rounded value, so a value just below a
/// unit boundary reads "1.00 MB", never "1000 KB" or "100.0 MB".
pub fn bytes(n: u64) -> String {
    format::bytes(n)
}

/// The disk the capacity tiles describe.
#[cfg(target_os = "macos")]
pub const STARTUP_DISK: &str = "the startup disk";
#[cfg(not(target_os = "macos"))]
pub const STARTUP_DISK: &str = "the system drive";

/// Caption of a folder that adds up to more than the disk uses: shared data
/// is counted in full per name (APFS clones on macOS, hard links outside the
/// Windows folder on Windows, as Explorer counts them).
#[cfg(target_os = "macos")]
pub const MORE_THAN_DISK: &str = "More than the disk uses · cloned files count in full";
#[cfg(not(target_os = "macos"))]
pub const MORE_THAN_DISK: &str = "More than the drive uses · hard-linked files count per name";

/// Startup-disk capacity as Finder reports it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Capacity {
    pub disk: DiskSpace,
    /// Space macOS can reclaim on demand (included in `disk.free`), when known.
    pub purgeable: Option<u64>,
}

/// Text of the three overview tiles.
#[derive(Clone, Debug, PartialEq)]
pub struct Overview {
    pub used_value: String,
    pub used_caption: String,
    /// 0.0…1.0 for the meter; `None` when the disk could not be read.
    pub used_fraction: Option<f64>,
    pub available_value: String,
    pub available_caption: String,
    pub folder_title: String,
    pub folder_value: String,
    pub folder_caption: String,
}

/// `capacity`: `None` while reading, `Err` when it failed.
/// `folder_total`: measured size of the folder shown, if measured.
/// `on_startup_disk`: whether that folder lives on the startup disk (a
/// percentage of the startup disk is meaningless for other volumes).
pub fn overview(
    capacity: Option<&Result<Capacity, String>>,
    folder_title: &str,
    folder_path: &str,
    folder_total: Option<u64>,
    on_startup_disk: bool,
) -> Overview {
    let folder_value = folder_total
        .map(format::bytes)
        .unwrap_or_else(|| "—".into());
    match capacity {
        Some(Ok(c)) => {
            let d = c.disk;
            let f = d.used_fraction();
            let available_caption = match c.purgeable {
                Some(p) if p > 0 => {
                    format!("Free on {STARTUP_DISK}, incl. {} purgeable", bytes(p))
                }
                _ => format!("Free on {STARTUP_DISK}"),
            };
            let folder_caption = match folder_total {
                None => "Not measured yet".into(),
                Some(_) if !on_startup_disk => folder_path.to_string(),
                // Cloned and otherwise shared files are counted in full per file (as Finder
                // does), so a big enough folder can add up to more than the disk uses.
                Some(s) if d.used() > 0 && s > d.used() => MORE_THAN_DISK.into(),
                Some(s) if d.used() > 0 => format!("{} of used space", share(s, d.used())),
                Some(_) => folder_path.to_string(),
            };
            Overview {
                used_value: bytes(d.used()),
                used_caption: format!(
                    "{} of {} · {}",
                    share_fraction(f, d.used() > 0),
                    bytes(d.total),
                    d.status().label()
                ),
                used_fraction: Some(f),
                available_value: bytes(d.free),
                available_caption,
                folder_title: folder_title.to_string(),
                folder_value,
                folder_caption,
            }
        }
        other => {
            let caption = if other.is_none() {
                "Reading disk…"
            } else {
                "Unavailable"
            };
            Overview {
                used_value: "—".into(),
                used_caption: caption.into(),
                used_fraction: None,
                available_value: "—".into(),
                available_caption: caption.into(),
                folder_title: folder_title.to_string(),
                folder_value,
                folder_caption: folder_path.to_string(),
            }
        }
    }
}

/// Share `part` of `whole` as a label that never rounds a non-empty item to
/// "0%" nor a partial one up to "100%".
pub fn share(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "0%".into();
    }
    share_fraction(part as f64 / whole as f64, part > 0)
}

pub fn share_fraction(fraction: f64, non_empty: bool) -> String {
    let f = fraction.clamp(0.0, 1.0);
    let pct = (f * 100.0).round();
    if non_empty && pct < 1.0 {
        "<1%".into()
    } else if f < 1.0 && pct >= 100.0 {
        "99%".into()
    } else {
        format!("{pct:.0}%")
    }
}

/// Toolbar summary for a measured folder: "12 items · 3.2 GB".
pub fn folder_summary(listing: &Listing) -> String {
    format!(
        "{} · {}",
        format::count(listing.entries.len(), "item"),
        bytes(listing.total)
    )
}

/// "1 file" / "1,234 files"
pub fn count_files(n: u64) -> String {
    if n == 1 {
        "1 file".into()
    } else {
        format!("{} files", engine::group_digits(n))
    }
}

/// Second line of a folder row.
pub fn child_subtitle(e: &Entry) -> String {
    let mut subtitle = match e.kind {
        ItemKind::Folder | ItemKind::Package => {
            format!("{} · {}", e.kind.label(), count_files(e.files))
        }
        ItemKind::File => match e.modified {
            Some(m) => format!("File · Modified {}", engine::format_date(m)),
            None => "File".into(),
        },
        ItemKind::Symlink => match &e.alias_of {
            Some(target) => format!(
                "{} to {} · its contents are not counted here",
                e.kind.label(),
                engine::display_path(target)
            ),
            None => format!("{} · its contents are not counted here", e.kind.label()),
        },
        other => other.label().to_string(),
    };
    if e.mounted {
        subtitle = "Mounted volume · its contents are stored elsewhere and not counted here".into();
    }
    if e.unreadable > 0 {
        subtitle.push_str(" · Partly unreadable");
    }
    if e.protection.is_some() {
        subtitle.push_str(" · Protected");
    }
    subtitle
}

/// Right-aligned percentage of a row (share of the folder's total).
pub fn child_percent(listing: &Listing, e: &Entry) -> String {
    share_fraction(listing.fraction(e.size), e.size > 0)
}

/// Note under a truncated list, or `None` when every item is shown.
pub fn more_note(listing: &Listing, shown: usize) -> Option<String> {
    if shown >= listing.entries.len() {
        return None;
    }
    let rest: u64 = listing.entries[shown..].iter().map(|e| e.size).sum();
    Some(format!(
        "Showing the {shown} largest of {} items. The other items take {}.",
        engine::group_digits(listing.entries.len() as u64),
        bytes(rest)
    ))
}

/// Section description of the large-files list.
pub fn large_description(root: &str) -> String {
    format!(
        "Files using {} or more on disk in {root}",
        bytes(engine::LARGE_FILE_THRESHOLD)
    )
}

/// Second line of a large-file row.
pub fn large_subtitle(f: &LargeFile) -> String {
    let folder = f
        .path
        .parent()
        .map(engine::display_path)
        .unwrap_or_default();
    let mut subtitle = match f.modified {
        Some(m) => format!("{folder} · Modified {}", engine::format_date(m)),
        None => folder,
    };
    if f.protection.is_some() {
        subtitle.push_str(" · Protected");
    }
    subtitle
}

/// Live detail line while a folder is being measured.
pub fn scan_detail(p: &engine::ScanProgress) -> String {
    if p.total == 0 {
        return "Reading folder…".to_string();
    }
    let mut d = format!("{} of {} items", p.done, p.total);
    if !p.current.is_empty() {
        d = format!("{} · {d}", p.current);
    }
    format!(
        "{d} · {} files · {}",
        engine::group_digits(p.files),
        bytes(p.bytes)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cap(total: u64, free: u64, purgeable: Option<u64>) -> Result<Capacity, String> {
        Ok(Capacity {
            disk: DiskSpace { total, free },
            purgeable,
        })
    }

    #[test]
    fn share_never_rounds_to_zero_or_hundred_wrongly() {
        assert_eq!(share(0, 100), "0%");
        assert_eq!(share(1, 1000), "<1%");
        assert_eq!(share(4, 1000), "<1%");
        assert_eq!(share(5, 1000), "1%");
        assert_eq!(share(994, 1000), "99%");
        assert_eq!(share(996, 1000), "99%");
        assert_eq!(share(1000, 1000), "100%");
        assert_eq!(share(50, 0), "0%");
        assert_eq!(share_fraction(0.504, true), "50%");
    }

    #[test]
    fn overview_texts() {
        let c = cap(1_000_000_000_000, 100_000_000_000, Some(5_000_000_000));
        let o = overview(Some(&c), "Home", "~", Some(450_000_000_000), true);
        assert_eq!(o.used_value, "900 GB");
        assert_eq!(o.used_caption, "90% of 1.00 TB · Almost full");
        assert_eq!(o.available_value, "100 GB");
        assert_eq!(
            o.available_caption,
            format!("Free on {STARTUP_DISK}, incl. 5.00 GB purgeable")
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            o.available_caption,
            "Free on the startup disk, incl. 5.00 GB purgeable"
        );
        let no_purgeable = cap(1_000_000_000_000, 100_000_000_000, None);
        let o2 = overview(Some(&no_purgeable), "Home", "~", None, true);
        assert_eq!(o2.available_caption, format!("Free on {STARTUP_DISK}"));
        assert_eq!(o.folder_value, "450 GB");
        assert_eq!(o.folder_caption, "50% of used space");

        // A folder on another volume is not a share of the startup disk.
        let o = overview(
            Some(&c),
            "Backup",
            "/Volumes/Backup",
            Some(450_000_000_000),
            false,
        );
        assert_eq!(o.folder_caption, "/Volumes/Backup");

        // Cloned files are counted in full per file, so a folder can exceed the disk's used space.
        let o = overview(Some(&c), "Macintosh HD", "/", Some(950_000_000_000), true);
        assert_eq!(o.folder_caption, MORE_THAN_DISK);
        #[cfg(target_os = "macos")]
        assert_eq!(
            o.folder_caption,
            "More than the disk uses · cloned files count in full"
        );

        let o = overview(Some(&c), "Home", "~", None, true);
        assert_eq!(o.folder_value, "—");
        assert_eq!(o.folder_caption, "Not measured yet");

        let o = overview(None, "Home", "~", None, true);
        assert_eq!(o.used_caption, "Reading disk…");
        let err: Result<Capacity, String> = Err("boom".into());
        let o = overview(Some(&err), "Home", "~", Some(1000), true);
        assert_eq!(
            (o.used_value.as_str(), o.used_caption.as_str()),
            ("—", "Unavailable")
        );
        assert_eq!(o.folder_caption, "~");
    }

    #[test]
    fn bytes_round_before_choosing_the_unit() {
        assert_eq!(bytes(0), "0 bytes");
        assert_eq!(bytes(1), "1 byte");
        assert_eq!(bytes(999), "999 bytes");
        assert_eq!(bytes(1_000), "1 KB");
        assert_eq!(bytes(999_499), "999 KB");
        assert_eq!(bytes(999_500), "1.00 MB");
        assert_eq!(bytes(999_999), "1.00 MB");
        assert_eq!(bytes(1_500_000), "1.50 MB");
        assert_eq!(bytes(9_996_000), "10.0 MB");
        assert_eq!(bytes(99_950_000), "100 MB");
        assert_eq!(bytes(99_940_000), "99.9 MB");
        assert_eq!(bytes(999_600_000), "1.00 GB");
        assert_eq!(bytes(54_070_000_000), "54.1 GB");
        assert_eq!(bytes(974_160_000_000), "974 GB");
        assert_eq!(bytes(999_960_000_000), "1.00 TB");
        assert_eq!(bytes(994_662_584_320), "995 GB");
        // Same as the shared formatter wherever no unit boundary is involved.
        for n in [
            1_234u64,
            56_789,
            123_456,
            7_654_321,
            87_654_321,
            345_678_901,
            4_567_890_123,
        ] {
            assert_eq!(bytes(n), format::bytes(n), "{n}");
        }
    }

    #[test]
    fn used_caption_never_claims_a_full_disk_early() {
        let c = cap(1_000_000_000_000, 4_000_000_000, None); // 99.6% used
        let o = overview(Some(&c), "Home", "~", None, true);
        assert_eq!(o.used_caption, "99% of 1.00 TB · Almost full");
    }

    #[test]
    fn alias_and_mount_subtitles() {
        let mut e = Entry {
            path: PathBuf::from("/x/Desktop"),
            name: "Desktop".into(),
            kind: ItemKind::Symlink,
            size: 0,
            files: 1,
            unreadable: 0,
            modified: None,
            protection: None,
            mounted: false,
            alias_of: Some(PathBuf::from("/Volumes/Data/Desktop")),
        };
        #[cfg(target_os = "macos")]
        assert_eq!(
            child_subtitle(&e),
            "Alias to /Volumes/Data/Desktop · its contents are not counted here"
        );
        #[cfg(windows)]
        {
            e.alias_of = Some(PathBuf::from(r"\\?\D:\Data\Desktop"));
            assert_eq!(
                child_subtitle(&e),
                r"Link to D:\Data\Desktop · its contents are not counted here"
            );
        }
        e.kind = ItemKind::Folder;
        e.alias_of = None;
        e.files = 0;
        e.mounted = true;
        assert_eq!(
            child_subtitle(&e),
            "Mounted volume · its contents are stored elsewhere and not counted here"
        );
    }

    #[test]
    fn row_texts() {
        let mut e = Entry {
            path: PathBuf::from("/x/a"),
            name: "a".into(),
            kind: ItemKind::Folder,
            size: 1_500_000,
            files: 1,
            unreadable: 2,
            modified: None,
            protection: Some("why"),
            mounted: false,
            alias_of: None,
        };
        assert_eq!(
            child_subtitle(&e),
            "Folder · 1 file · Partly unreadable · Protected"
        );
        e.files = 12_345;
        e.unreadable = 0;
        e.protection = None;
        assert_eq!(child_subtitle(&e), "Folder · 12,345 files");
        let listing = Listing {
            path: PathBuf::from("/x"),
            entries: vec![e.clone()],
            total: 3_000_000,
            unreadable: 0,
            on_startup_disk: true,
        };
        assert_eq!(child_percent(&listing, &e), "50%");
        assert_eq!(folder_summary(&listing), "1 item · 3.00 MB");
        assert!(more_note(&listing, 1).is_none());
        assert_eq!(
            more_note(&listing, 0).unwrap(),
            "Showing the 0 largest of 1 items. The other items take 1.50 MB."
        );
        assert_eq!(
            large_description("~/Movies"),
            "Files using 500 MB or more on disk in ~/Movies"
        );
    }
}
