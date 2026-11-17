//! Windows half of the engine: sizes as File Explorer reports them (with
//! hard links counted once), hard-link identity from the file system's file
//! IDs, finding git without a console window, and the hidden attribute.

use super::Measured;
use nera_core::fs_util::{self, Cancel};
use std::collections::HashMap;
use std::os::windows::fs::MetadataExt as _;
use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
/// Junctions, directory symlinks, mount points (and cloud placeholders).
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
/// `CREATE_NO_WINDOW`: a console program started from the GUI app would
/// otherwise flash a console window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Git for Windows (or any `git.exe` on `PATH`). Only absolute `PATH` entries
/// are searched, so a `git.exe` in the current folder is never run.
pub(super) fn git_binary() -> Option<&'static Path> {
    static GIT: OnceLock<Option<PathBuf>> = OnceLock::new();
    GIT.get_or_init(|| {
        let on_path = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join("git.exe"));
        let installed = [
            ("ProgramFiles", r"Git\cmd\git.exe"),
            ("ProgramW6432", r"Git\cmd\git.exe"),
            ("ProgramFiles(x86)", r"Git\cmd\git.exe"),
            ("LOCALAPPDATA", r"Programs\Git\cmd\git.exe"),
        ]
        .into_iter()
        .filter_map(|(var, rel)| Some(PathBuf::from(std::env::var_os(var)?).join(rel)));
        on_path.chain(installed).find(|p| p.is_file())
    })
    .as_deref()
}

/// Run git without opening a console window.
pub(super) fn no_window(cmd: &mut Command) {
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// Windows hides items with the HIDDEN attribute (e.g. `AppData`, `.git`,
/// `desktop.ini`), whatever their name. Free: read from the directory listing.
pub(super) fn is_hidden(entry: &std::fs::DirEntry) -> bool {
    entry
        .metadata()
        .is_ok_and(|m| m.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0)
}

/// Size of `path` as File Explorer's "Size" reports it (file lengths; real
/// allocation for compressed, sparse and cloud-only files), except that a
/// file with several hard links inside the folder is counted once, as it
/// occupies the disk once. `unique` is what deleting the folder is guaranteed
/// to free: it leaves out hard-linked files that keep a link outside `path`
/// (pnpm's content store, cargo's uplifted binaries).
///
/// Hard links can only be seen by opening each file (`fs_util::file_key`),
/// so that cost is paid here, once per non-empty file inside a junk folder,
/// and never during the project search. Junctions and directory symlinks are
/// never followed (npm and pnpm create them inside `node_modules`).
///
/// Data shared through ReFS block cloning (Dev Drive) is not detected.
pub fn measure(path: &Path, cancel: &Cancel) -> Measured {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Measured::default();
    };
    if !meta.is_dir() {
        let size = fs_util::allocated_size_at(path, &meta);
        return match fs_util::file_key(path, &meta) {
            Some(key) if key.links > 1 => Measured::default(),
            _ => Measured { size, unique: size },
        };
    }
    // Folders hold no data of their own in Explorer's figures.
    let mut total = Measured::default();
    // (volume, file index) -> (links seen inside, total links, bytes)
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
            // From the directory listing: does not follow links, costs nothing.
            let Ok(meta) = entry.metadata() else { continue };
            if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 && !meta.is_file() {
                continue; // junction, directory symlink or mount point
            }
            if meta.is_dir() {
                if depth < 64 {
                    stack.push((entry.path(), depth + 1));
                }
                continue;
            }
            if !meta.is_file() {
                continue; // file symlink: holds no data
            }
            let child = entry.path();
            let bytes = fs_util::allocated_size_at(&child, &meta);
            if bytes == 0 {
                continue;
            }
            match fs_util::file_key(&child, &meta) {
                Some(key) if key.links > 1 => {
                    let slot = linked.entry(key.id()).or_insert((0, key.links, bytes));
                    slot.0 += 1;
                }
                _ => {
                    total.size += bytes;
                    total.unique += bytes;
                }
            }
        }
    }
    let every_link: u64 = linked.values().map(|(_, _, bytes)| bytes).sum();
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
