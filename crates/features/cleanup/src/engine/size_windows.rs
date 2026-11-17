//! Sizing of what a cleanup really frees, on Windows. Same questions and the
//! same public API as `size.rs` (macOS); the answers come from Windows:
//!
//! * A file counts what File Explorer's "Size" shows for it
//!   (`allocated_size_at`: the length, or the real allocation of sparse,
//!   compressed and OneDrive placeholder files). Folders hold no data.
//! * A hard-linked file frees its data only when every link is removed, so it
//!   counts once, and only when all its links lie inside the sized set.
//! * There is no owner rule to emulate: everything in your profile is yours to
//!   remove. Folders outside it (`Windows\Temp`, the system error reports)
//!   only count when Nera runs as administrator; otherwise their contents are
//!   measured as blocked. Files another program holds open cannot be removed
//!   either, but that changes from one minute to the next, so they are counted
//!   here and a cleanup reports them as skipped.
//! * The Recycle Bin is measured by Windows (`SHQueryRecycleBinW`).
//!
//! The same code sizes the scan and the "freed" figure of a cleanup, so the
//! two never disagree.

use super::targets::is_trash_dir;
use nera_core::fs_util::{self, Cancel};
use nera_core::paths::{home, is_under, standardize};
use std::collections::HashMap;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// How an item leaves its folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removal {
    /// Moved into the Recycle Bin in one piece.
    Trash,
    /// Deleted recursively.
    Permanent,
}

/// May this user remove items at `item` without an administrator?
fn removable_location(item: &Path, home: &Path, elevated: bool) -> bool {
    elevated || is_under(item, home)
}

struct Link {
    seen: u64,
    nlink: u64,
    bytes: u64,
}

/// Accumulates the bytes of a set of items. One `Sizer` per category.
pub struct Sizer<'a> {
    cancel: &'a Cancel,
    home: PathBuf,
    elevated: bool,
    /// Measure the opposite of the normal question: the bytes that are there
    /// but that this user may NOT remove.
    blocked_mode: bool,
    bytes: u64,
    links: HashMap<(u64, u64), Link>,
    /// Kept for the shared API: on Windows an unreadable folder is reported
    /// per target by the scan, not from inside a walk.
    pub denied: Vec<PathBuf>,
    visited: usize,
}

impl<'a> Sizer<'a> {
    pub fn new(cancel: &'a Cancel) -> Self {
        Self {
            cancel,
            home: standardize(home()),
            elevated: super::win::is_elevated(),
            blocked_mode: false,
            bytes: 0,
            links: HashMap::new(),
            denied: Vec::new(),
            visited: 0,
        }
    }

    /// A sizer that counts what cannot be removed (see `blocked_mode`).
    pub fn blocked(cancel: &'a Cancel) -> Self {
        Self {
            blocked_mode: true,
            ..Self::new(cancel)
        }
    }

    /// Bytes freed by everything added so far.
    pub fn total(&self) -> u64 {
        let linked: u64 = self
            .links
            .values()
            .filter(|l| l.seen >= l.nlink)
            .map(|l| l.bytes)
            .sum();
        self.bytes + linked
    }

    /// Fold the result of another sizer (same category) into this one.
    pub fn merge(&mut self, other: Sizer<'_>) {
        self.bytes += other.bytes;
        for (key, link) in other.links {
            self.links
                .entry(key)
                .and_modify(|l| l.seen += link.seen)
                .or_insert(link);
        }
        for d in other.denied {
            if self.denied.len() < 64 {
                self.denied.push(d);
            }
        }
    }

    /// Add one item (file or folder) that a cleanup removes whole.
    pub fn add_item(&mut self, item: &Path, _removal: Removal) {
        if is_trash_dir(item) {
            if !self.blocked_mode {
                self.bytes += super::win::recycle_bin().map_or(0, |(bytes, _)| bytes);
            }
            return;
        }
        let Ok(meta) = std::fs::symlink_metadata(item) else {
            return;
        };
        if !removable_location(item, &self.home, self.elevated) {
            if self.blocked_mode {
                self.add_occupied(item, &meta);
            }
            return;
        }
        if self.blocked_mode {
            return; // everything here is removable
        }
        if meta.is_dir() {
            self.walk(item);
        } else {
            self.add_file(item, &meta);
        }
    }

    /// Everything under an item that cannot be removed.
    fn add_occupied(&mut self, item: &Path, meta: &std::fs::Metadata) {
        if !meta.is_dir() {
            self.bytes += fs_util::allocated_size_at(item, meta);
            return;
        }
        let mut stack = vec![item.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                if self.tick() {
                    return;
                }
                let Ok(m) = entry.metadata() else { continue };
                if enterable(&m) {
                    stack.push(entry.path());
                } else {
                    self.bytes += fs_util::allocated_size_at(&entry.path(), &m);
                }
            }
        }
    }

    fn add_file(&mut self, path: &Path, meta: &std::fs::Metadata) {
        let bytes = fs_util::allocated_size_at(path, meta);
        if bytes == 0 {
            return; // nothing to free, and no need to open the file
        }
        match fs_util::file_key(path, meta) {
            Some(key) if key.links > 1 => {
                let link = self.links.entry(key.id()).or_insert(Link {
                    seen: 0,
                    nlink: key.links,
                    bytes,
                });
                link.seen += 1;
            }
            _ => self.bytes += bytes,
        }
    }

    fn walk(&mut self, root: &Path) {
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            // An unreadable folder inside a target (made by an elevated
            // installer) cannot be emptied either; a cleanup reports it.
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if self.tick() {
                    return;
                }
                let Ok(meta) = entry.metadata() else { continue };
                if enterable(&meta) {
                    stack.push(entry.path());
                } else {
                    self.add_file(&entry.path(), &meta);
                }
            }
        }
    }

    /// Count one visited entry; true when the scan was cancelled.
    fn tick(&mut self) -> bool {
        self.visited += 1;
        self.visited.is_multiple_of(512) && self.cancel.is_cancelled()
    }
}

/// A real folder: junctions, mount points and directory links are removed as
/// links, never entered, so what they point to is not counted.
fn enterable(meta: &std::fs::Metadata) -> bool {
    meta.is_dir() && meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
}

/// Bytes freed by removing `item` (file or folder) the given way.
pub fn item_bytes(item: &Path, removal: Removal, cancel: &Cancel) -> u64 {
    let mut sizer = Sizer::new(cancel);
    sizer.add_item(item, removal);
    sizer.total()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nera-size-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(p: &Path, n: usize) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, vec![7u8; n]).unwrap();
    }

    fn size(p: &Path, removal: Removal) -> u64 {
        item_bytes(p, removal, &Cancel::new())
    }

    #[test]
    fn plain_files_count_their_explorer_size() {
        let root = tmp("plain");
        write(&root.join(r"a\b\c.bin"), 1_000_000);
        write(&root.join(r"a\.hidden\d.bin"), 500_000);
        assert_eq!(size(&root.join("a"), Removal::Permanent), 1_500_000);
        assert_eq!(size(&root.join(r"a\b\c.bin"), Removal::Trash), 1_000_000);
        assert_eq!(size(&root.join("missing"), Removal::Permanent), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hard_links_count_once_and_only_when_all_links_are_inside() {
        let root = tmp("links");
        write(&root.join(r"in\a.bin"), 1_000_000);
        std::fs::hard_link(root.join(r"in\a.bin"), root.join(r"in\b.bin")).unwrap();
        write(&root.join(r"mixed\x.bin"), 1_000_000);
        std::fs::hard_link(root.join(r"mixed\x.bin"), root.join("keep.bin")).unwrap();
        assert_eq!(size(&root.join("in"), Removal::Permanent), 1_000_000);
        assert_eq!(
            size(&root.join("mixed"), Removal::Permanent),
            0,
            "another link outside keeps the data"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn locations_outside_the_profile_need_an_administrator() {
        let home = Path::new(r"C:\Users\me");
        assert!(removable_location(
            Path::new(r"C:\Users\me\AppData\Local\Temp\x"),
            home,
            false
        ));
        assert!(removable_location(
            Path::new(r"c:\users\ME\AppData\Local\Temp\x"),
            home,
            false
        ));
        assert!(!removable_location(
            Path::new(r"C:\Windows\Temp\x"),
            home,
            false
        ));
        assert!(removable_location(
            Path::new(r"C:\Windows\Temp\x"),
            home,
            true
        ));
        assert!(!removable_location(
            Path::new(r"C:\Users\other\x"),
            home,
            false
        ));
    }

    #[test]
    fn read_only_files_are_removed_and_counted() {
        let root = tmp("readonly");
        let file = root.join(r"d\ro.bin");
        write(&file, 10_000);
        let mut perms = std::fs::metadata(&file).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&file, perms).unwrap();
        std::fs::copy(&file, root.join("top.bin")).unwrap();
        assert_eq!(size(&root.join("d"), Removal::Permanent), 10_000);
        // Both a lone read-only file and one inside a folder go.
        fs_util::remove_item(&root.join("top.bin")).unwrap();
        fs_util::remove_item(&root.join("d")).unwrap();
        assert!(!root.join("d").exists() && !root.join("top.bin").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn junction_targets_are_not_counted() {
        let root = tmp("junction");
        write(&root.join(r"target\big.bin"), 1_000_000);
        std::fs::create_dir_all(root.join("item")).unwrap();
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(root.join(r"item\link"))
            .arg(root.join("target"))
            .output()
            .is_ok_and(|o| o.status.success());
        if made {
            assert_eq!(size(&root.join("item"), Removal::Permanent), 0);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn blocked_bytes_are_zero_inside_the_profile() {
        let root = tmp("blocked");
        write(&root.join(r"rw\inner.bin"), 1_000);
        let cancel = Cancel::new();
        let mut sizer = Sizer::blocked(&cancel);
        sizer.add_item(&root.join("rw"), Removal::Permanent);
        assert_eq!(sizer.total(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }
}
