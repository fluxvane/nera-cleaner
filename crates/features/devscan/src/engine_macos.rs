//! macOS half of the engine: sizes like `du` (allocated blocks, hard links
//! from `st_nlink`, APFS clones from the private-size attribute) and finding a
//! real git. The code is the original macOS implementation, moved here unchanged.

use super::Measured;
use nera_core::fs_util::{self, Cancel};
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// A real git (never the `/usr/bin/git` shim, which pops up an install dialog
/// when the Command Line Tools are missing).
pub(super) fn git_binary() -> Option<&'static Path> {
    static GIT: OnceLock<Option<PathBuf>> = OnceLock::new();
    GIT.get_or_init(|| {
        [
            "/Library/Developer/CommandLineTools/usr/bin/git",
            "/Applications/Xcode.app/Contents/Developer/usr/bin/git",
            "/opt/homebrew/bin/git",
            "/usr/local/bin/git",
        ]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
    })
    .as_deref()
}

/// Nothing to do on macOS: git runs without a window.
pub(super) fn no_window(_cmd: &mut Command) {}

/// macOS marks hidden items by their leading dot, which the walk checks itself.
pub(super) fn is_hidden(_entry: &std::fs::DirEntry) -> bool {
    false
}

/// Size of `path` on disk, exactly what `du -sk` reports: allocated blocks, a
/// file with several hard links counted once (cargo links `target/debug/x` to
/// `target/debug/deps/x`). `unique` is what deleting the folder is guaranteed
/// to free: it leaves out hard-linked files that keep a link outside `path`
/// (e.g. into pnpm's content store) and data shared with APFS clones, using the
/// file system's own private-size figure.
pub fn measure(path: &Path, cancel: &Cancel) -> Measured {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Measured::default();
    };
    let own = |meta: &std::fs::Metadata| fs_util::allocated_size(meta);
    if !meta.is_dir() {
        return if meta.nlink() > 1 {
            Measured::default()
        } else {
            file_measure(path, &meta)
        };
    }
    let mut total = Measured {
        size: own(&meta),
        unique: own(&meta),
    };
    // (device, inode) -> (links seen inside, total links, allocated bytes)
    let mut linked: HashMap<(u64, u64), (u64, u64, u64)> = HashMap::new();
    let mut stack = vec![(path.to_path_buf(), 0usize)];
    let mut visited = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited.is_multiple_of(512) && cancel.is_cancelled() {
                return total;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                total.size += own(&meta);
                total.unique += own(&meta);
                if depth < 64 {
                    stack.push((entry.path(), depth + 1));
                }
            } else if meta.nlink() > 1 && meta.is_file() {
                let slot =
                    linked
                        .entry((meta.dev(), meta.ino()))
                        .or_insert((0, meta.nlink(), own(&meta)));
                slot.0 += 1;
            } else {
                let m = file_measure(&entry.path(), &meta);
                total.size += m.size;
                total.unique += m.unique;
            }
        }
    }
    // `du` counts every hard-linked file once, wherever its other links are.
    let every_link: u64 = linked.values().map(|(_, _, bytes)| bytes).sum();
    // Deleting the folder frees such a file only when no link survives outside.
    let all_inside: u64 = linked
        .values()
        .filter(|(inside, links, _)| inside >= links)
        .map(|(_, _, bytes)| bytes)
        .sum();
    Measured {
        size: total.size + every_link,
        unique: total.unique + all_inside,
    }
}

fn file_measure(path: &Path, meta: &std::fs::Metadata) -> Measured {
    let size = fs_util::allocated_size(meta);
    let unique = if meta.is_file() && size > 0 {
        private_size(path).map_or(size, |p| p.min(size))
    } else {
        size
    };
    Measured { size, unique }
}

/// Bytes only this file holds (APFS `ATTR_CMNEXT_PRIVATESIZE`): what deleting
/// it alone would free. Files that share extents with a clone report less than
/// their allocated size. `None` when the volume cannot say.
pub fn private_size(path: &Path) -> Option<u64> {
    use std::ffi::{c_char, c_int, c_void, CString};
    use std::os::unix::ffi::OsStrExt as _;

    #[repr(C)]
    struct AttrList {
        bitmapcount: u16,
        reserved: u16,
        commonattr: u32,
        volattr: u32,
        dirattr: u32,
        fileattr: u32,
        forkattr: u32,
    }
    #[repr(C, packed(4))]
    struct Reply {
        length: u32,
        private_size: i64,
    }
    extern "C" {
        fn getattrlist(
            path: *const c_char,
            list: *mut c_void,
            buf: *mut c_void,
            size: usize,
            options: u32,
        ) -> c_int;
    }
    const ATTR_BIT_MAP_COUNT: u16 = 5;
    const ATTR_CMNEXT_PRIVATESIZE: u32 = 0x0000_0008;
    const FSOPT_NOFOLLOW: u32 = 0x0000_0001;
    const FSOPT_ATTR_CMN_EXTENDED: u32 = 0x0000_0020;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut list = AttrList {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: ATTR_CMNEXT_PRIVATESIZE,
    };
    let mut reply = Reply {
        length: 0,
        private_size: -1,
    };
    // SAFETY: `list` and `reply` are valid for the sizes passed; the kernel
    // writes at most `size_of::<Reply>()` bytes and reads `c_path` as a C string.
    let rc = unsafe {
        getattrlist(
            c_path.as_ptr(),
            (&mut list as *mut AttrList).cast(),
            (&mut reply as *mut Reply).cast(),
            std::mem::size_of::<Reply>(),
            FSOPT_NOFOLLOW | FSOPT_ATTR_CMN_EXTENDED,
        )
    };
    let private = reply.private_size;
    (rc == 0 && reply.length as usize >= std::mem::size_of::<Reply>() && private >= 0)
        .then_some(private as u64)
}
