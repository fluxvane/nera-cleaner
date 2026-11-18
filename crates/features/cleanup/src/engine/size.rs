//! Sizing of what a cleanup really frees.
//!
//! `du` and `st_blocks` answer "how much does this folder occupy", which is
//! not what the user wants to know. This module answers "how many bytes come
//! back if the cleanup removes this item":
//!
//! * APFS clones share their data, so each file is sized by its PRIVATE size
//!   (`ATTR_CMNEXT_PRIVATESIZE`): the bytes that are freed when only that file
//!   goes. Non-APFS volumes fall back to the allocated size (`st_blocks`).
//! * A hard-linked file frees its data only when every link is removed, so it
//!   counts once, and only when all its links lie inside the sized set.
//! * Entries this user may not unlink (root-owned parent, sticky-bit owned by
//!   someone else, immutable or SIP-restricted flags) are not counted.
//!
//! The same code sizes the scan and the "freed" figure of a cleanup, so the
//! two never disagree.

use nera_core::fs_util::Cancel;
use std::collections::HashMap;
use std::ffi::CString;
use std::os::macos::fs::MetadataExt as _;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

/// How an item leaves its folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removal {
    /// Moved (renamed) into the Trash in one piece: everything inside leaves
    /// with it, whatever the permissions of nested folders.
    Trash,
    /// Deleted recursively: only entries the user may unlink go.
    Permanent,
}

/// UF_IMMUTABLE | UF_APPEND | UF_NOUNLINK | SF_IMMUTABLE | SF_APPEND |
/// SF_RESTRICTED | SF_NOUNLINK: the item cannot be removed from its folder.
const LOCKED_FLAGS: u32 = 0x2 | 0x4 | 0x10 | 0x2_0000 | 0x4_0000 | 0x8_0000 | 0x10_0000;

/// The same flags minus the two NOUNLINK ones: on a FOLDER they only stop the
/// folder itself from being unlinked (the per-user `T` folder has one), not
/// its entries from being removed.
const FOLDER_LOCKED_FLAGS: u32 = 0x2 | 0x4 | 0x2_0000 | 0x4_0000 | 0x8_0000;

/// Bytes freed by removing this single file or symlink.
pub fn file_bytes(path: &Path, meta: &std::fs::Metadata) -> u64 {
    let allocated = meta.blocks() * 512;
    if allocated == 0 {
        return 0;
    }
    private_size(path).map_or(allocated, |p| p.min(allocated))
}

/// `ATTR_CMNEXT_PRIVATESIZE`: bytes that would be freed by deleting only this
/// file (clone-shared extents excluded). `None` on volumes without support.
fn private_size(path: &Path) -> Option<u64> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: zeroed attrlist is a valid "no attributes" request; fields are set below.
    let mut list: libc::attrlist = unsafe { std::mem::zeroed() };
    list.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    list.forkattr = libc::ATTR_CMNEXT_PRIVATESIZE;
    #[repr(C, packed)]
    struct Buf {
        len: u32,
        size: i64,
    }
    let mut buf = Buf { len: 0, size: 0 };
    // SAFETY: valid C string, attrlist and a buffer of the stated size.
    let rc = unsafe {
        libc::getattrlist(
            c.as_ptr(),
            &mut list as *mut libc::attrlist as *mut libc::c_void,
            &mut buf as *mut Buf as *mut libc::c_void,
            std::mem::size_of::<Buf>(),
            libc::FSOPT_NOFOLLOW | libc::FSOPT_ATTR_CMN_EXTENDED,
        )
    };
    let (len, size) = (buf.len, buf.size);
    (rc == 0 && len as usize >= std::mem::size_of::<Buf>() && size >= 0).then_some(size as u64)
}

fn euid() -> u32 {
    // SAFETY: no arguments, cannot fail.
    unsafe { libc::geteuid() }
}

fn is_writable_dir(path: &Path) -> bool {
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: valid C string.
    unsafe { libc::access(c.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

/// What this user may do inside one directory.
#[derive(Clone, Copy, Debug)]
struct DirPerm {
    writable: bool,
    sticky: bool,
    owner: u32,
}

impl DirPerm {
    fn of(path: &Path, meta: &std::fs::Metadata, me: u32) -> Self {
        let locked = meta.st_flags() & FOLDER_LOCKED_FLAGS != 0;
        Self {
            writable: me == 0 || (!locked && is_writable_dir(path)),
            sticky: meta.mode() & 0o1000 != 0,
            owner: meta.uid(),
        }
    }

    /// May this user unlink `child` from the directory?
    fn can_unlink(&self, child: &std::fs::Metadata, me: u32) -> bool {
        if !self.writable {
            return false;
        }
        if me != 0 && child.st_flags() & LOCKED_FLAGS != 0 {
            return false;
        }
        !self.sticky || me == 0 || child.uid() == me || self.owner == me
    }
}

struct Link {
    seen: u64,
    nlink: u64,
    bytes: u64,
}

/// Accumulates the bytes of a set of items. One `Sizer` per category.
pub struct Sizer<'a> {
    cancel: &'a Cancel,
    me: u32,
    /// Measure the opposite of the normal question: the bytes that are there
    /// but that this user may NOT remove (allocated size, hard links per link).
    blocked_mode: bool,
    bytes: u64,
    links: HashMap<(u64, u64), Link>,
    /// Folders of this user that could not be read (privacy protection).
    pub denied: Vec<PathBuf>,
    visited: usize,
    parent: Option<(PathBuf, DirPerm)>,
}

impl<'a> Sizer<'a> {
    pub fn new(cancel: &'a Cancel) -> Self {
        Self {
            cancel,
            me: euid(),
            blocked_mode: false,
            bytes: 0,
            links: HashMap::new(),
            denied: Vec::new(),
            visited: 0,
            parent: None,
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
    pub fn add_item(&mut self, item: &Path, removal: Removal) {
        let Ok(meta) = std::fs::symlink_metadata(item) else {
            return;
        };
        let Some(parent) = item.parent() else { return };
        let cached = self
            .parent
            .as_ref()
            .filter(|(p, _)| p == parent)
            .map(|(_, perm)| *perm);
        let perm = match cached {
            Some(perm) => perm,
            None => {
                let Ok(parent_meta) = std::fs::symlink_metadata(parent) else {
                    return;
                };
                let perm = DirPerm::of(parent, &parent_meta, self.me);
                self.parent = Some((parent.to_path_buf(), perm));
                perm
            }
        };
        if !perm.can_unlink(&meta, self.me) {
            if self.blocked_mode {
                self.add_occupied(item, &meta);
            }
            return;
        }
        if !meta.is_dir() {
            if !self.blocked_mode {
                self.add_file(item, &meta);
            }
            return;
        }
        self.walk(item, &meta, removal);
    }

    /// Everything under an item that cannot be removed, by allocated size.
    fn add_occupied(&mut self, item: &Path, meta: &std::fs::Metadata) {
        self.bytes += meta.blocks() * 512;
        if !meta.is_dir() {
            return;
        }
        let mut stack = vec![item.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                self.visited += 1;
                if self.visited.is_multiple_of(512) && self.cancel.is_cancelled() {
                    return;
                }
                let Ok(m) = entry.metadata() else { continue };
                self.bytes += m.blocks() * 512;
                if m.is_dir() {
                    stack.push(entry.path());
                }
            }
        }
    }

    fn add_file(&mut self, path: &Path, meta: &std::fs::Metadata) {
        let bytes = file_bytes(path, meta);
        if meta.nlink() > 1 && !meta.is_dir() {
            let link = self.links.entry((meta.dev(), meta.ino())).or_insert(Link {
                seen: 0,
                nlink: meta.nlink(),
                bytes,
            });
            link.seen += 1;
        } else {
            self.bytes += bytes;
        }
    }

    fn walk(&mut self, root: &Path, root_meta: &std::fs::Metadata, removal: Removal) {
        if !self.blocked_mode {
            self.bytes += root_meta.blocks() * 512;
        }
        let mut stack = vec![(root.to_path_buf(), DirPerm::of(root, root_meta, self.me))];
        while let Some((dir, perm)) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::PermissionDenied {
                        self.note_denied(&dir);
                    }
                    continue;
                }
            };
            for entry in entries.flatten() {
                self.visited += 1;
                if self.visited.is_multiple_of(512) && self.cancel.is_cancelled() {
                    return;
                }
                let path = entry.path();
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_dir() {
                    if !self.blocked_mode {
                        self.bytes += meta.blocks() * 512;
                    }
                    // Even a folder that cannot be unlinked can hold removable contents.
                    stack.push((path.clone(), DirPerm::of(&path, &meta, self.me)));
                } else if removal == Removal::Trash || perm.can_unlink(&meta, self.me) {
                    if !self.blocked_mode {
                        self.add_file(&path, &meta);
                    }
                } else if self.blocked_mode {
                    self.bytes += meta.blocks() * 512;
                }
            }
        }
    }

    fn note_denied(&mut self, dir: &Path) {
        let ours = std::fs::symlink_metadata(dir).is_ok_and(|m| m.uid() == self.me);
        if ours && self.denied.len() < 64 {
            self.denied.push(dir.to_path_buf());
        }
    }
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
    use std::os::unix::fs::PermissionsExt;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nera-size-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(p: &Path, n: usize) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        // Synced, so APFS has allocated the blocks before a test sizes the
        // file (otherwise it measures 0 now and then).
        let mut f = std::fs::File::create(p).unwrap();
        std::io::Write::write_all(&mut f, &vec![7u8; n]).unwrap();
        f.sync_all().unwrap();
    }

    fn size(p: &Path, removal: Removal) -> u64 {
        item_bytes(p, removal, &Cancel::new())
    }

    #[test]
    fn plain_files_match_allocated_size() {
        let root = tmp("plain");
        write(&root.join("a/b/c.bin"), 1_000_000);
        write(&root.join("a/.hidden/d.bin"), 500_000);
        let meta = std::fs::metadata(root.join("a/b/c.bin")).unwrap();
        assert_eq!(
            file_bytes(&root.join("a/b/c.bin"), &meta),
            meta.blocks() * 512
        );
        let total = size(&root.join("a"), Removal::Permanent);
        assert!((1_500_000..1_600_000).contains(&total), "{total}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hard_links_count_once_and_only_when_all_links_are_inside() {
        let root = tmp("links");
        write(&root.join("in/a.bin"), 1_000_000);
        std::fs::hard_link(root.join("in/a.bin"), root.join("in/b.bin")).unwrap();
        write(&root.join("mixed/x.bin"), 1_000_000);
        std::fs::hard_link(root.join("mixed/x.bin"), root.join("keep.bin")).unwrap();
        let inside = size(&root.join("in"), Removal::Permanent);
        assert!(
            (1_000_000..1_100_000).contains(&inside),
            "counted once: {inside}"
        );
        assert_eq!(
            size(&root.join("mixed"), Removal::Permanent),
            0,
            "another link outside keeps the data"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clones_free_nothing_until_the_last_copy_goes() {
        let root = tmp("clone");
        write(&root.join("orig.bin"), 8_000_000);
        // Only meaningful where the temp dir is on APFS (`cp -c` = clonefile).
        let cloned = std::process::Command::new("cp")
            .arg("-c")
            .arg(root.join("orig.bin"))
            .arg(root.join("copy.bin"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let independent = root.join("indep.bin");
        write(&independent, 8_000_000);
        if cloned {
            // Both clones share every extent: deleting just one frees nothing.
            assert_eq!(size(&root.join("copy.bin"), Removal::Permanent), 0);
            assert_eq!(size(&root.join("orig.bin"), Removal::Permanent), 0);
            // Known limit: deleting BOTH frees the data, but each is sized alone.
        }
        let alone = size(&independent, Removal::Permanent);
        assert!(alone >= 8_000_000, "a normal file counts in full: {alone}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unwritable_folders_are_not_counted_for_permanent_removal() {
        let root = tmp("perm");
        write(&root.join("ro/inner.bin"), 1_000_000);
        write(&root.join("rw/inner.bin"), 1_000_000);
        std::fs::set_permissions(root.join("ro"), std::fs::Permissions::from_mode(0o555)).unwrap();
        let permanent = size(&root, Removal::Permanent);
        let moved = size(&root, Removal::Trash);
        std::fs::set_permissions(root.join("ro"), std::fs::Permissions::from_mode(0o755)).unwrap();
        // The root itself is removable here; ro/inner.bin is not unlinkable.
        assert!((1_000_000..1_100_000).contains(&permanent), "{permanent}");
        assert!((2_000_000..2_200_000).contains(&moved), "{moved}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_nounlink_folder_still_lets_its_entries_go() {
        // The per-user `T` folder carries SF_NOUNLINK; its entries are removable.
        let t = std::env::temp_dir();
        let flags = std::fs::metadata(&t).map(|m| m.st_flags()).unwrap_or(0);
        let probe = t.join(format!("nera-nounlink-{}", std::process::id()));
        std::fs::create_dir_all(&probe).unwrap();
        std::fs::write(probe.join("f"), vec![1u8; 100_000]).unwrap();
        assert!(
            size(&probe, Removal::Permanent) >= 100_000,
            "temp dir flags {flags:#x}"
        );
        std::fs::remove_dir_all(probe).unwrap();
    }

    #[test]
    fn unreadable_own_folder_is_reported_as_denied() {
        let root = tmp("denied");
        write(&root.join("locked/x.bin"), 1_000);
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let cancel = Cancel::new();
        let mut sizer = Sizer::new(&cancel);
        sizer.add_item(&root.join("locked"), Removal::Permanent);
        let denied = sizer.denied.clone();
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert_eq!(denied, vec![root.join("locked")]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn system_owned_items_are_not_counted() {
        // /Library/Updates style: root-owned folder in a root-owned parent.
        if euid() == 0 {
            return;
        }
        assert_eq!(size(Path::new("/private/var/log"), Removal::Permanent), 0);
    }

    fn blocked(p: &Path, removal: Removal) -> u64 {
        let cancel = Cancel::new();
        let mut sizer = Sizer::blocked(&cancel);
        sizer.add_item(p, removal);
        sizer.total()
    }

    #[test]
    fn blocked_bytes_are_what_cannot_be_removed() {
        let root = tmp("blocked");
        write(&root.join("ro/inner.bin"), 1_000_000);
        write(&root.join("rw/inner.bin"), 1_000_000);
        std::fs::set_permissions(root.join("ro"), std::fs::Permissions::from_mode(0o555)).unwrap();
        let permanent = blocked(&root, Removal::Permanent);
        let moved = blocked(&root, Removal::Trash);
        std::fs::set_permissions(root.join("ro"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            (1_000_000..1_100_000).contains(&permanent),
            "only ro/inner.bin: {permanent}"
        );
        assert_eq!(moved, 0, "moving the whole item takes everything with it");
        // Everything removable: nothing is blocked.
        assert_eq!(blocked(&root.join("rw"), Removal::Permanent), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn system_logs_are_blocked_not_empty() {
        if euid() == 0 {
            return;
        }
        let logs = Path::new("/private/var/log");
        assert_eq!(size(logs, Removal::Permanent), 0);
        assert!(blocked(logs, Removal::Permanent) > 0);
    }
}
