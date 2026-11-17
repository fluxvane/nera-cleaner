//! Space Lens engine: measures the immediate children of a folder (each one
//! recursively, in parallel), finds large files under a root, and keeps a
//! per-path cache of results consistent after an item is moved to the Trash.
//!
//! Port of the Swift `AnalyzeDiskSpaceUseCase` + `ScannerActor`
//! (`loadChildren`, `calculateFolderSizes`, `scanLargeFiles`). No UI here.
//!
//! OS-specific pieces (sizes, volume boundaries, hard-link identity, safety
//! reasons, capacity, display names) live in `engine/macos.rs` and
//! `engine/windows.rs`, which export the same names.

use nera_core::fs_util::{allocated_size, Cancel};
use nera_core::paths::standardize;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs::{DirEntry, Metadata};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

pub use platform::{
    allocated, display_path, folder_name, has_full_disk_access, on_startup_disk, protection_reason,
};
#[cfg(target_os = "macos")]
pub use platform::{available_for_important_usage, FULL_DISK_ACCESS_URL};
use platform::{
    dir_kind, is_volume_mount, link_key, lists_volumes, tracks_links, trash_dirs, volume_id,
    volume_set, LINK_LABEL, SKIP_SUBTREES,
};

/// Files at least this large appear under "Large files" (decimal, like Finder).
pub const LARGE_FILE_THRESHOLD: u64 = 500 * 1000 * 1000;
/// How many large files are kept (largest first).
pub const LARGE_FILE_LIMIT: usize = 50;

/// Report progress after this many files have been counted.
const PROGRESS_EVERY: u64 = 2048;

/// Lock shards of the hard-link table.
const LINK_SHARDS: usize = 16;

// MARK: - Model

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemKind {
    Folder,
    /// A bundle Finder shows as one item (Photos library, archive, …).
    Package,
    /// An application bundle (`.app`).
    App,
    File,
    Symlink,
    /// Socket, FIFO, device node.
    Other,
}

impl ItemKind {
    /// True when the item can be opened in Space Lens to see its contents.
    pub fn is_drillable(self) -> bool {
        matches!(self, ItemKind::Folder | ItemKind::Package)
    }

    pub fn label(self) -> &'static str {
        match self {
            ItemKind::Folder => "Folder",
            ItemKind::Package => "Package",
            ItemKind::App => "Application",
            ItemKind::File => "File",
            ItemKind::Symlink => LINK_LABEL,
            ItemKind::Other => "Special file",
        }
    }
}

/// Lucide icon for an item, by kind and (for files) extension.
pub fn icon_for(kind: ItemKind, name: &str) -> &'static str {
    match kind {
        ItemKind::Folder => "folder",
        ItemKind::Package => "package",
        ItemKind::App => "app-window",
        ItemKind::Symlink => "file-symlink",
        ItemKind::Other => "file",
        ItemKind::File => {
            let ext = extension(name);
            match ext.as_str() {
                "mov" | "mp4" | "m4v" | "mkv" | "avi" | "webm" | "mpg" | "mpeg" => {
                    "file-video-camera"
                }
                "mp3" | "m4a" | "aac" | "wav" | "aiff" | "aif" | "flac" | "ogg" => "file-music",
                "jpg" | "jpeg" | "png" | "gif" | "heic" | "tiff" | "tif" | "raw" | "dng"
                | "psd" | "webp" => "file-image",
                "zip" | "tar" | "gz" | "tgz" | "xz" | "bz2" | "7z" | "rar" | "dmg" | "iso"
                | "pkg" | "xip" | "img" => "file-archive",
                "txt" | "md" | "pdf" | "doc" | "docx" | "rtf" | "csv" | "log" => "file-text",
                "rs" | "swift" | "js" | "ts" | "py" | "json" | "sql" | "db" | "sqlite" => {
                    "file-code"
                }
                _ => "file",
            }
        }
    }
}

fn extension(name: &str) -> String {
    Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Classify a directory entry from its `lstat` metadata.
pub fn classify(path: &Path, meta: &Metadata) -> ItemKind {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ft = meta.file_type();
    if ft.is_symlink() {
        ItemKind::Symlink
    } else if ft.is_dir() {
        dir_kind(path, &extension(&name))
    } else if ft.is_file() {
        ItemKind::File
    } else {
        ItemKind::Other
    }
}

/// One immediate child of a measured folder.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub kind: ItemKind,
    /// Allocated bytes on disk (recursive for folders).
    pub size: u64,
    /// Files counted inside (1 for a file).
    pub files: u64,
    /// Sub-folders that could not be read; `size` is then a lower bound.
    pub unreadable: u64,
    pub modified: Option<SystemTime>,
    /// Why it can't be moved to the Trash (see [`protection_reason`]),
    /// computed off the UI thread when measured.
    pub protection: Option<&'static str>,
    /// A volume mounted inside the folder: its contents live elsewhere (disk
    /// image, other disk) and are not counted here, so `size` is just the
    /// mount point itself.
    pub mounted: bool,
    /// Where an alias (symbolic link) points. Links are never followed, so the
    /// target's size is not part of `size`.
    pub alias_of: Option<PathBuf>,
}

/// The measured contents of one folder, largest first.
#[derive(Clone, Debug, PartialEq)]
pub struct Listing {
    pub path: PathBuf,
    pub entries: Vec<Entry>,
    pub total: u64,
    pub unreadable: u64,
    /// The folder lives on the startup disk, so a share "of used space" of
    /// that disk is meaningful for it.
    pub on_startup_disk: bool,
}

impl Listing {
    /// Share of the folder's total taken by `size` (0.0…1.0).
    pub fn fraction(&self, size: u64) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            (size as f64 / self.total as f64).clamp(0.0, 1.0)
        }
    }

    fn sort(&mut self) {
        sort_entries(&mut self.entries);
    }
}

fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.size
            .cmp(&a.size)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// Live progress of [`list_children`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScanProgress {
    /// Children fully measured.
    pub done: usize,
    /// Children to measure.
    pub total: usize,
    /// Child most recently started.
    pub current: String,
    pub files: u64,
    pub bytes: u64,
}

impl ScanProgress {
    pub fn fraction(&self) -> Option<f64> {
        (self.total > 0).then(|| self.done as f64 / self.total as f64)
    }
}

/// Live progress of [`find_large_files`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LargeProgress {
    pub files: u64,
    pub bytes: u64,
    pub found: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanError {
    Cancelled,
    NotFound,
    NotADirectory,
    PermissionDenied,
    Io(String),
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::Cancelled => write!(f, "The scan was cancelled."),
            ScanError::NotFound => write!(f, "This folder no longer exists."),
            ScanError::NotADirectory => write!(f, "This item is not a folder."),
            ScanError::PermissionDenied => {
                write!(
                    f,
                    concat!(
                        nera_core::os_name!(),
                        " doesn't allow Nera Cleaner to read this folder."
                    )
                )
            }
            ScanError::Io(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ScanError {}

fn io_error(err: std::io::Error) -> ScanError {
    match err.kind() {
        std::io::ErrorKind::NotFound => ScanError::NotFound,
        std::io::ErrorKind::PermissionDenied => ScanError::PermissionDenied,
        _ => ScanError::Io(err.to_string()),
    }
}

// MARK: - Walking

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Tally {
    size: u64,
    files: u64,
    unreadable: u64,
}

impl std::ops::Add for Tally {
    type Output = Tally;
    fn add(self, o: Tally) -> Tally {
        Tally {
            size: self.size + o.size,
            files: self.files + o.files,
            unreadable: self.unreadable + o.unreadable,
        }
    }
}

/// Collects large items while walking.
struct LargeCollector<'a> {
    threshold: u64,
    limit: usize,
    items: Mutex<Vec<LargeFile>>,
    found: &'a AtomicUsize,
}

impl LargeCollector<'_> {
    fn offer(&self, path: &Path, kind: ItemKind, size: u64, modified: Option<SystemTime>) {
        if size < self.threshold {
            return;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.found.fetch_add(1, Ordering::Relaxed);
        let protection = protection_reason(path);
        let mut items = self.items.lock().unwrap();
        items.push(LargeFile {
            path: path.to_path_buf(),
            name,
            kind,
            size,
            modified,
            protection,
        });
        // Bound memory: keep only the current top-N now and then.
        if items.len() > self.limit * 4 {
            sort_large(&mut items);
            items.truncate(self.limit);
        }
    }
}

struct Walker<'a> {
    cancel: &'a Cancel,
    skip: Vec<PathBuf>,
    files: AtomicU64,
    bytes: AtomicU64,
    on_tick: &'a (dyn Fn(u64, u64) + Sync),
    large: Option<LargeCollector<'a>>,
}

/// `(device, inode)` of hard-linked files already counted inside one measured
/// folder, so a file with several names is sized once there (as `du -sk
/// <folder>` does). Every immediate child of a listed folder gets its own set:
/// a row then always equals the folder's own total, whatever the order in which
/// siblings are walked. Sharded to limit contention between worker threads.
struct Links([Mutex<HashSet<(u64, u64)>>; LINK_SHARDS]);

impl Links {
    fn new() -> Self {
        Self(std::array::from_fn(|_| Mutex::new(HashSet::new())))
    }

    /// Bytes `meta` (allocated `size`) adds to the folder total: its size,
    /// except that every further hard link to an already counted file adds nothing.
    /// `key` gives the file's identity when it has more than one name.
    fn counted_size(
        &self,
        meta: &Metadata,
        size: u64,
        key: impl FnOnce() -> Option<(u64, u64)>,
    ) -> u64 {
        if size == 0 || meta.is_dir() {
            return size;
        }
        let Some(key) = key() else {
            return size;
        };
        let fresh = self.0[(key.1 % LINK_SHARDS as u64) as usize]
            .lock()
            .unwrap()
            .insert(key);
        if fresh {
            size
        } else {
            0
        }
    }
}

fn entry_allocated(entry: &DirEntry, meta: &Metadata) -> u64 {
    allocated(|| entry.path(), meta)
}

impl<'a> Walker<'a> {
    fn new(root: &Path, cancel: &'a Cancel, on_tick: &'a (dyn Fn(u64, u64) + Sync)) -> Self {
        Self {
            cancel,
            skip: skip_list(root),
            files: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            on_tick,
            large: None,
        }
    }

    fn skipped(&self, path: &Path) -> bool {
        self.skip.iter().any(|s| s == path)
    }

    fn count(&self, files: u64, bytes: u64) {
        if files == 0 && bytes == 0 {
            return;
        }
        let before = self.files.fetch_add(files, Ordering::Relaxed);
        let total_bytes = self.bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if before / PROGRESS_EVERY != (before + files) / PROGRESS_EVERY {
            (self.on_tick)(before + files, total_bytes);
        }
    }

    /// Recursive allocated size of a directory, sub-folders in parallel.
    /// `in_package` suppresses large-file reporting inside a bundle that is
    /// reported as a whole.
    fn walk(&self, dir: &Path, in_package: bool, links: &Links, devs: &[u64]) -> Tally {
        if self.cancel.is_cancelled() {
            return Tally::default();
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            // Gone since its parent was read: nothing to count, nothing unreadable.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Tally::default(),
            Err(_) => {
                return Tally {
                    unreadable: 1,
                    ..Tally::default()
                }
            }
        };
        let mut own = Tally::default();
        let mut subdirs: Vec<(PathBuf, Metadata)> = Vec::new();
        let track_links = tracks_links(dir);
        for entry in entries.flatten() {
            // DirEntry::metadata does not follow symlinks.
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                let path = entry.path();
                if !self.skipped(&path) && devs.contains(&volume_id(&meta)) {
                    subdirs.push((path, meta));
                }
                continue;
            }
            let size = entry_allocated(&entry, &meta);
            own.size += if track_links {
                links.counted_size(&meta, size, || link_key(&entry, &meta))
            } else {
                size
            };
            own.files += 1;
            if !in_package && meta.is_file() {
                if let Some(large) = &self.large {
                    // A file is "large" by the space it takes on disk, even
                    // when it is hard-linked elsewhere.
                    if size >= large.threshold {
                        let path = entry.path();
                        large.offer(&path, classify(&path, &meta), size, meta.modified().ok());
                    }
                }
            }
        }
        self.count(own.files, own.size);

        let nested = subdirs
            .par_iter()
            .map(|(path, meta)| {
                let kind = classify(path, meta);
                let package = !in_package
                    && self.large.is_some()
                    && matches!(kind, ItemKind::App | ItemKind::Package);
                let mut t = self.walk(path, in_package || package, links, devs);
                t.size += allocated_size(meta);
                if package {
                    if let Some(large) = &self.large {
                        large.offer(path, kind, t.size, meta.modified().ok());
                    }
                }
                t
            })
            .reduce(Tally::default, |a, b| a + b);
        own + nested
    }
}

/// Exact paths never descended into while scanning `root`.
fn skip_list(root: &Path) -> Vec<PathBuf> {
    let root_std = standardize(root);
    SKIP_SUBTREES
        .iter()
        .map(PathBuf::from)
        .filter(|s| !root_std.starts_with(s))
        .collect()
}

/// Measure every immediate child of `dir` (folders recursively, all in
/// parallel). Results are sorted largest first.
pub fn list_children(
    dir: &Path,
    cancel: &Cancel,
    on_progress: &(dyn Fn(ScanProgress) + Sync),
) -> Result<Listing, ScanError> {
    let meta = std::fs::metadata(dir).map_err(io_error)?;
    if !meta.is_dir() {
        return Err(ScanError::NotADirectory);
    }
    let read = std::fs::read_dir(dir).map_err(io_error)?;

    let current = Mutex::new(String::new());
    let done = AtomicUsize::new(0);
    let total = AtomicUsize::new(0);
    let report = |files: u64, bytes: u64| {
        on_progress(ScanProgress {
            done: done.load(Ordering::Relaxed),
            total: total.load(Ordering::Relaxed),
            current: current.lock().unwrap().clone(),
            files,
            bytes,
        })
    };
    let walker = Walker::new(dir, cancel, &report);

    let children: Vec<(PathBuf, String, Metadata)> = read
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            let path = e.path();
            if walker.skipped(&path) {
                return None;
            }
            Some((path, e.file_name().to_string_lossy().into_owned(), meta))
        })
        .collect();
    total.store(children.len(), Ordering::Relaxed);
    report(0, 0);

    let base_devs = volume_set(dir);
    // Listing /Volumes shows the mounted disks themselves, each measured on its own.
    let listing_volumes = lists_volumes(dir);
    let mut entries: Vec<Entry> = children
        .into_par_iter()
        .map(|(path, name, meta)| {
            // Windows: a volume mounted on a folder is a link-like reparse point.
            let mut mounted = is_volume_mount(&path, &meta);
            let kind = if mounted {
                ItemKind::Folder
            } else {
                classify(&path, &meta)
            };
            let tally = if meta.is_dir() {
                *current.lock().unwrap() = name.clone();
                let own_devs;
                let devs: &[u64] = if base_devs.contains(&volume_id(&meta)) {
                    &base_devs
                } else if listing_volumes {
                    own_devs = [base_devs.as_slice(), &[volume_id(&meta)]].concat();
                    &own_devs
                } else {
                    mounted = true;
                    &[]
                };
                let mut t = if mounted {
                    Tally::default()
                } else {
                    walker.walk(&path, false, &Links::new(), devs)
                };
                t.size += allocated_size(&meta);
                t
            } else {
                let size = allocated(|| path.clone(), &meta);
                walker.count(1, size);
                Tally {
                    size,
                    files: 1,
                    unreadable: 0,
                }
            };
            let alias_of = (!mounted && meta.file_type().is_symlink())
                .then(|| std::fs::read_link(&path).ok())
                .flatten()
                .map(|t| dir.join(t));
            done.fetch_add(1, Ordering::Relaxed);
            report(
                walker.files.load(Ordering::Relaxed),
                walker.bytes.load(Ordering::Relaxed),
            );
            let protection = protection_reason(&path);
            Entry {
                path,
                name,
                kind,
                size: tally.size,
                files: tally.files,
                unreadable: tally.unreadable,
                modified: meta.modified().ok(),
                protection,
                mounted,
                alias_of,
            }
        })
        .collect();

    if cancel.is_cancelled() {
        return Err(ScanError::Cancelled);
    }
    sort_entries(&mut entries);
    let total = entries.iter().map(|e| e.size).sum();
    let unreadable = entries.iter().map(|e| e.unreadable).sum();
    Ok(Listing {
        path: dir.to_path_buf(),
        entries,
        total,
        unreadable,
        on_startup_disk: on_startup_disk(dir),
    })
}

// MARK: - Large files

/// A file (or bundle counted as one item) of at least the threshold size.
#[derive(Clone, Debug, PartialEq)]
pub struct LargeFile {
    pub path: PathBuf,
    pub name: String,
    pub kind: ItemKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// Why it can't be moved to the Trash, computed when found.
    pub protection: Option<&'static str>,
}

fn sort_large(items: &mut [LargeFile]) {
    items.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
}

/// Files of at least `threshold` bytes anywhere under `root`, largest first,
/// at most `limit`. App bundles and packages (Photos library, archives, …)
/// count as single items.
pub fn find_large_files(
    root: &Path,
    threshold: u64,
    limit: usize,
    cancel: &Cancel,
    on_progress: &(dyn Fn(LargeProgress) + Sync),
) -> Result<Vec<LargeFile>, ScanError> {
    let meta = std::fs::metadata(root).map_err(io_error)?;
    if !meta.is_dir() {
        return Err(ScanError::NotADirectory);
    }
    std::fs::read_dir(root).map_err(io_error)?;

    let found = AtomicUsize::new(0);
    let report = |files: u64, bytes: u64| {
        on_progress(LargeProgress {
            files,
            bytes,
            found: found.load(Ordering::Relaxed),
        })
    };
    let mut walker = Walker::new(root, cancel, &report);
    // Items already in the Trash cannot be moved there again.
    walker.skip.extend(trash_dirs(root));
    walker.large = Some(LargeCollector {
        threshold,
        limit,
        items: Mutex::new(Vec::new()),
        found: &found,
    });
    walker.walk(root, false, &Links::new(), &volume_set(root));
    if cancel.is_cancelled() {
        return Err(ScanError::Cancelled);
    }
    let mut items = walker
        .large
        .map(|l| l.items.into_inner().unwrap())
        .unwrap_or_default();
    sort_large(&mut items);
    items.truncate(limit);
    Ok(items)
}

// MARK: - Cache upkeep

/// Keep cached listings consistent after `removed` (of `size` bytes) went to
/// the Trash: drop it from its parent's listing, shrink every cached ancestor,
/// and forget listings of anything inside it.
pub fn apply_removal(cache: &mut HashMap<PathBuf, Listing>, removed: &Path, size: u64) {
    cache.retain(|path, _| !path.starts_with(removed));
    for listing in cache.values_mut() {
        if !removed.starts_with(&listing.path) || removed == listing.path {
            continue;
        }
        if let Some(i) = listing.entries.iter().position(|e| e.path == removed) {
            let entry = listing.entries.remove(i);
            listing.total = listing.total.saturating_sub(entry.size);
        } else if let Some(entry) = listing
            .entries
            .iter_mut()
            .find(|e| removed.starts_with(&e.path))
        {
            entry.size = entry.size.saturating_sub(size);
            listing.total = listing.total.saturating_sub(size);
        }
        listing.sort();
    }
}

/// Store a freshly measured listing and bring the cached listings above it up
/// to date: a re-measured folder may have grown or shrunk since its parent was
/// measured, and the parent (and every cached ancestor) must show the new size.
pub fn merge_remeasured(cache: &mut HashMap<PathBuf, Listing>, fresh: Listing) {
    fn shift(v: u64, delta: i128) -> u64 {
        (v as i128 + delta).max(0) as u64
    }
    let mut child = fresh.path.clone();
    // First step sets the folder's entry to the fresh measurement; every step
    // above shifts by how much the step below changed.
    let mut absolute = Some((
        fresh.total,
        fresh.entries.iter().map(|e| e.files).sum::<u64>(),
    ));
    let mut delta = (0i128, 0i128);
    cache.insert(child.clone(), fresh);
    while let Some(parent) = child.parent().map(Path::to_path_buf) {
        let Some(listing) = cache.get_mut(&parent) else {
            break;
        };
        let Some(entry) = listing.entries.iter_mut().find(|e| e.path == child) else {
            break;
        };
        let (new_size, new_files) = match absolute.take() {
            Some(abs) => abs,
            None => (shift(entry.size, delta.0), shift(entry.files, delta.1)),
        };
        delta = (
            new_size as i128 - entry.size as i128,
            new_files as i128 - entry.files as i128,
        );
        if delta == (0, 0) {
            break;
        }
        entry.size = new_size;
        entry.files = new_files;
        listing.total = shift(listing.total, delta.0);
        listing.sort();
        child = parent;
    }
}

/// Drop large files that were `removed` or lived inside it.
pub fn prune_large(items: &mut Vec<LargeFile>, removed: &Path) {
    items.retain(|f| !f.path.starts_with(removed));
}

/// Forget cached listings of `path` and everything inside it.
pub fn invalidate(cache: &mut HashMap<PathBuf, Listing>, path: &Path) {
    cache.retain(|p, _| !p.starts_with(path));
}

// MARK: - Disk capacity

/// Capacity of the startup disk as Finder reports it: `free` includes
/// purgeable space (caches macOS clears on demand); the raw `statfs` free
/// figure is used when macOS gives no purgeable figure. On Windows: the system
/// drive's free and total space as Explorer shows them (no purgeable figure).
pub fn startup_capacity() -> Result<crate::view::Capacity, String> {
    let v = nera_core::disk::startup_volume().map_err(|e| e.to_string())?;
    Ok(crate::view::Capacity {
        disk: v.space,
        purgeable: v.purgeable,
    })
}

// MARK: - Display helpers

/// Breadcrumb trail from `root` down to `current` (inclusive). When
/// `current` is not inside `root`, the trail is just `current`.
pub fn breadcrumb(root: &Path, current: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rest) = current.strip_prefix(root) else {
        return vec![(folder_name(current), current.to_path_buf())];
    };
    let mut trail = vec![(folder_name(root), root.to_path_buf())];
    let mut at = root.to_path_buf();
    for comp in rest.components() {
        at.push(comp);
        trail.push((comp.as_os_str().to_string_lossy().into_owned(), at.clone()));
    }
    trail
}

/// Parent of `current` when it stays within `root`.
pub fn parent_within(root: &Path, current: &Path) -> Option<PathBuf> {
    if current == root {
        return None;
    }
    current
        .parent()
        .filter(|p| p.starts_with(root))
        .map(Path::to_path_buf)
}

/// "1,234,567"
pub fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// "Mar 4, 2025"
pub fn format_date(t: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Local> = t.into();
    dt.format("%b %-d, %Y").to_string()
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    use super::platform::{attr_alloc_size, UF_COMPRESSED};
    use super::*;
    use nera_core::paths::home;
    #[cfg(target_os = "macos")]
    use std::os::macos::fs::MetadataExt as _;
    #[cfg(target_os = "macos")]
    use std::os::unix::fs::MetadataExt as _;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("nera-space-{tag}-{}", std::process::id()));
            if dir.exists() {
                std::fs::remove_dir_all(&dir).unwrap();
            }
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn file(&self, rel: &str, len: usize) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, vec![7u8; len]).unwrap();
            p
        }
    }

    impl Drop for TempDir {
        // Test fixture cleanup inside the system temp dir only.
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn noop(_: ScanProgress) {}

    #[cfg(target_os = "macos")]
    #[test]
    fn lists_children_sorted_by_size_with_kinds() {
        let t = TempDir::new("list");
        t.file("big/a.bin", 300_000);
        t.file("big/nested/deeper/b.bin", 200_000);
        t.file("small/c.txt", 10_000);
        t.file("loose.mov", 100_000);
        t.file("Tool.app/Contents/MacOS/tool", 50_000);
        std::os::unix::fs::symlink(t.0.join("big"), t.0.join("link")).unwrap();

        let listing = list_children(&t.0, &Cancel::new(), &noop).unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(&names[..4], ["big", "loose.mov", "Tool.app", "small"]);

        let big = &listing.entries[0];
        assert_eq!(big.kind, ItemKind::Folder);
        assert!(big.size >= 500_000, "{}", big.size);
        assert_eq!(big.files, 2);
        assert_eq!(listing.entries[1].kind, ItemKind::File);
        assert_eq!(listing.entries[2].kind, ItemKind::App);
        assert!(!ItemKind::App.is_drillable());

        let link = listing.entries.iter().find(|e| e.name == "link").unwrap();
        assert_eq!(link.kind, ItemKind::Symlink);
        assert!(link.size < 100_000, "symlinks are not followed");

        assert_eq!(
            listing.total,
            listing.entries.iter().map(|e| e.size).sum::<u64>()
        );
        assert!(listing.fraction(big.size) > 0.5);
    }

    #[test]
    fn reports_progress_and_honours_cancel() {
        let t = TempDir::new("cancel");
        for i in 0..5 {
            t.file(&format!("d{i}/f"), 1000);
        }
        let last = Mutex::new(ScanProgress::default());
        let listing = list_children(&t.0, &Cancel::new(), &|p| *last.lock().unwrap() = p).unwrap();
        assert_eq!(listing.entries.len(), 5);
        let last = last.into_inner().unwrap();
        assert_eq!((last.done, last.total), (5, 5));
        assert_eq!(last.fraction(), Some(1.0));

        let cancel = Cancel::new();
        cancel.cancel();
        assert_eq!(
            list_children(&t.0, &cancel, &noop),
            Err(ScanError::Cancelled)
        );
    }

    #[test]
    fn errors_for_missing_and_non_folders() {
        let t = TempDir::new("errors");
        let f = t.file("plain", 10);
        assert_eq!(
            list_children(&t.0.join("missing"), &Cancel::new(), &noop),
            Err(ScanError::NotFound)
        );
        assert_eq!(
            list_children(&f, &Cancel::new(), &noop),
            Err(ScanError::NotADirectory)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn finds_large_files_and_counts_packages_once() {
        let t = TempDir::new("large");
        t.file("a/huge.dmg", 400_000);
        t.file("a/b/medium.mov", 250_000);
        t.file("tiny.txt", 1_000);
        t.file("Lib.photoslibrary/originals/x.heic", 150_000);
        t.file("Lib.photoslibrary/originals/y.heic", 150_000);

        let found = find_large_files(&t.0, 200_000, 10, &Cancel::new(), &|_| {}).unwrap();
        let names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["huge.dmg", "Lib.photoslibrary", "medium.mov"]);
        assert_eq!(found[1].kind, ItemKind::Package);
        assert!(found.windows(2).all(|w| w[0].size >= w[1].size));

        let top1 = find_large_files(&t.0, 200_000, 1, &Cancel::new(), &|_| {}).unwrap();
        assert_eq!(top1.len(), 1);
        assert_eq!(top1[0].name, "huge.dmg");

        let cancel = Cancel::new();
        cancel.cancel();
        assert_eq!(
            find_large_files(&t.0, 1, 10, &cancel, &|_| {}),
            Err(ScanError::Cancelled)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn hard_links_are_counted_once_like_du() {
        let t = TempDir::new("links");
        let one = t.file("a/data.bin", 400_000);
        std::fs::create_dir_all(t.0.join("a/inner")).unwrap();
        std::fs::hard_link(&one, t.0.join("a/inner/same.bin")).unwrap();
        t.file("c/other.bin", 100_000);

        let listing = list_children(&t.0, &Cancel::new(), &noop).unwrap();
        // 400 KB shared by two names + 100 KB, not 900 KB.
        assert!(
            listing.total >= 500_000 && listing.total < 700_000,
            "{}",
            listing.total
        );
        let files: u64 = listing.entries.iter().map(|e| e.files).sum();
        assert_eq!(files, 3, "every name still counts as a file");
    }

    #[test]
    fn rows_are_stable_when_siblings_share_a_hard_link() {
        let t = TempDir::new("siblings");
        let one = t.file("a/shared.bin", 400_000);
        std::fs::create_dir_all(t.0.join("b")).unwrap();
        std::fs::hard_link(&one, t.0.join("b/shared.bin")).unwrap();
        for _ in 0..20 {
            let listing = list_children(&t.0, &Cancel::new(), &noop).unwrap();
            // Each folder is sized on its own, exactly like `du -sk <folder>`.
            for e in &listing.entries {
                assert!(e.size >= 400_000, "{} = {}", e.name, e.size);
            }
            assert_eq!(listing.entries[0].size, listing.entries[1].size);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn large_files_use_allocated_size() {
        let t = TempDir::new("sparse");
        let sparse = t.0.join("sparse.img");
        // 50 MB logical size, (almost) nothing allocated: not a large file.
        std::fs::File::create(&sparse)
            .unwrap()
            .set_len(50_000_000)
            .unwrap();
        t.file("real.bin", 2_000_000);
        let found = find_large_files(&t.0, 1_000_000, 10, &Cancel::new(), &|_| {}).unwrap();
        let names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["real.bin"]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn app_folders_need_an_app_layout() {
        let t = TempDir::new("classify");
        t.file("Real.app/Contents/Info.plist", 10);
        t.file("Microsoft.NETCore.App/10.0.1/lib.dll", 10);
        let kind = |rel: &str| {
            let p = t.0.join(rel);
            classify(&p, &std::fs::symlink_metadata(&p).unwrap())
        };
        assert_eq!(kind("Real.app"), ItemKind::App);
        assert_eq!(kind("Microsoft.NETCore.App"), ItemKind::Folder);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn compressed_files_use_the_size_finder_reports() {
        // Values below are what NSURL (Finder, Spotlight) reports for these files.
        let probe = |path: &str, expected: u64| {
            let path = Path::new(path);
            let Ok(meta) = std::fs::symlink_metadata(path) else {
                return;
            };
            if meta.st_flags() & UF_COMPRESSED == 0 {
                return; // not the file this was measured on
            }
            assert_eq!(attr_alloc_size(path), Some(expected), "{}", path.display());
            assert_eq!(
                allocated(|| path.to_path_buf(), &meta),
                expected,
                "{}",
                path.display()
            );
        };
        // Compressed payload stored in the file's metadata: nothing allocated, though
        // st_blocks says one 4 KB block (this is what made `du` differ from Finder).
        probe(
            "/Applications/Telegram.app/Contents/Resources/currencies.json",
            0,
        );
        // A compressed file whose data is stored normally.
        probe("/usr/share/misc/ascii", 4096);

        // Uncompressed files keep st_blocks.
        let t = TempDir::new("plain");
        let plain_path = t.file("plain.bin", 10_000);
        let plain = std::fs::symlink_metadata(&plain_path).unwrap();
        assert_eq!(
            allocated(|| plain_path.clone(), &plain),
            allocated_size(&plain)
        );
        assert_eq!(attr_alloc_size(&plain_path), Some(allocated_size(&plain)));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn walks_stay_on_the_start_volume() {
        let t = TempDir::new("volumes");
        t.file("a/f", 10);
        let devs = volume_set(&t.0);
        let dev = std::fs::metadata(&t.0).unwrap().dev();
        assert!(devs.contains(&dev));
        // The system and data volumes of the startup disk are both allowed (firmlinks).
        assert!(devs.contains(&std::fs::metadata("/System/Volumes/Data").unwrap().dev()));
        // A volume mounted somewhere else is not: the Simulator runtimes mounted
        // under /Library/Developer are images stored elsewhere on the disk.
        if let Ok(rd) = std::fs::read_dir("/Library/Developer/CoreSimulator/Volumes") {
            for e in rd.flatten() {
                let m = e.metadata().unwrap();
                if !devs.contains(&m.dev()) {
                    let listing =
                        list_children(e.path().parent().unwrap(), &Cancel::new(), &noop).unwrap();
                    let entry = listing.entries.iter().find(|x| x.path == e.path()).unwrap();
                    assert!(entry.mounted && entry.size < 1_000_000, "{:?}", entry.size);
                }
            }
        }
    }

    #[test]
    fn remeasuring_updates_cached_ancestors() {
        let mut cache = HashMap::new();
        cache.insert(
            PathBuf::from("/r"),
            listing("/r", vec![entry("/r/a", 100), entry("/r/b", 80)]),
        );
        cache.insert(
            PathBuf::from("/r/a"),
            listing("/r/a", vec![entry("/r/a/x", 70), entry("/r/a/y", 30)]),
        );
        cache.insert(
            PathBuf::from("/r/a/x"),
            listing("/r/a/x", vec![entry("/r/a/x/1", 70)]),
        );

        // /r/a/x grew from 70 to 500 while its parents were cached.
        merge_remeasured(&mut cache, listing("/r/a/x", vec![entry("/r/a/x/1", 500)]));

        let a = &cache[Path::new("/r/a")];
        assert_eq!(a.entries[0].path, PathBuf::from("/r/a/x"));
        assert_eq!(a.entries[0].size, 500);
        assert_eq!(a.total, 530);
        let r = &cache[Path::new("/r")];
        assert_eq!(r.entries[0].path, PathBuf::from("/r/a"));
        assert_eq!(r.entries[0].size, 530, "100 + (500 - 70)");
        assert_eq!(r.total, 610);
        assert_eq!(cache[Path::new("/r/a/x")].total, 500);

        // Same size again: nothing above changes.
        merge_remeasured(&mut cache, listing("/r/a/x", vec![entry("/r/a/x/1", 500)]));
        assert_eq!(cache[Path::new("/r")].total, 610);
    }

    #[test]
    fn capacity_matches_the_volume() {
        let c = startup_capacity().unwrap();
        assert!(c.disk.total > 0 && c.disk.free <= c.disk.total);
        let raw = nera_core::disk::startup_disk().unwrap();
        assert_eq!(c.disk.total, raw.total);
        // Free space moves between the two reads; allow for that, not for a unit error.
        let expected = raw.free + c.purgeable.unwrap_or(0);
        assert!(
            c.disk.free.abs_diff(expected) < 512 * 1024 * 1024,
            "{} vs {expected}",
            c.disk.free
        );
        assert!(on_startup_disk(&home()));
        assert!(!on_startup_disk(Path::new("/definitely/not/here")));
    }

    fn entry(path: &str, size: u64) -> Entry {
        Entry {
            path: PathBuf::from(path),
            name: Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            kind: ItemKind::Folder,
            size,
            files: 1,
            unreadable: 0,
            modified: None,
            protection: None,
            mounted: false,
            alias_of: None,
        }
    }

    fn listing(path: &str, entries: Vec<Entry>) -> Listing {
        let total = entries.iter().map(|e| e.size).sum();
        Listing {
            path: PathBuf::from(path),
            entries,
            total,
            unreadable: 0,
            on_startup_disk: true,
        }
    }

    #[test]
    fn removal_updates_cached_ancestors() {
        let mut cache = HashMap::new();
        cache.insert(
            PathBuf::from("/r"),
            listing("/r", vec![entry("/r/a", 100), entry("/r/b", 80)]),
        );
        cache.insert(
            PathBuf::from("/r/a"),
            listing("/r/a", vec![entry("/r/a/x", 70), entry("/r/a/y", 30)]),
        );
        cache.insert(
            PathBuf::from("/r/a/x"),
            listing("/r/a/x", vec![entry("/r/a/x/1", 70)]),
        );

        apply_removal(&mut cache, Path::new("/r/a/x"), 70);

        assert!(!cache.contains_key(Path::new("/r/a/x")));
        let a = &cache[Path::new("/r/a")];
        assert_eq!(a.entries.len(), 1);
        assert_eq!(a.total, 30);
        let r = &cache[Path::new("/r")];
        assert_eq!(r.total, 110);
        // Re-sorted: b (80) now ahead of a (30).
        assert_eq!(r.entries[0].path, PathBuf::from("/r/b"));
        assert_eq!(r.entries[1].size, 30);

        let mut large = vec![
            LargeFile {
                path: "/r/a/x/1".into(),
                name: "1".into(),
                kind: ItemKind::File,
                size: 70,
                modified: None,
                protection: None,
            },
            LargeFile {
                path: "/r/b/2".into(),
                name: "2".into(),
                kind: ItemKind::File,
                size: 60,
                modified: None,
                protection: None,
            },
        ];
        prune_large(&mut large, Path::new("/r/a/x"));
        assert_eq!(large.len(), 1);

        invalidate(&mut cache, Path::new("/r/a"));
        assert_eq!(cache.len(), 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn protects_system_and_essential_folders() {
        let h = home();
        assert!(protection_reason(&h).is_some());
        assert!(protection_reason(Path::new("/System/Library")).is_some());
        assert!(protection_reason(Path::new("/Applications")).is_some());
        assert!(protection_reason(&h.join("Library")).is_some());
        assert!(protection_reason(&h.join("Documents")).is_some());
        assert!(protection_reason(&h.join(".Trash/old.zip")).is_some());
        assert!(protection_reason(&h.join("Downloads/movie.mov")).is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn breadcrumb_and_paths() {
        let root = Path::new("/a/b");
        let trail = breadcrumb(root, Path::new("/a/b/c/d"));
        let labels: Vec<&str> = trail.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["b", "c", "d"]);
        assert_eq!(trail[2].1, PathBuf::from("/a/b/c/d"));
        assert_eq!(breadcrumb(root, root).len(), 1);
        assert_eq!(breadcrumb(root, Path::new("/x")).len(), 1);

        assert_eq!(
            parent_within(root, Path::new("/a/b/c")),
            Some(PathBuf::from("/a/b"))
        );
        assert_eq!(parent_within(root, root), None);

        assert_eq!(folder_name(&home()), "Home");
        assert_eq!(folder_name(Path::new("/")), "Macintosh HD");
        assert_eq!(display_path(&home().join("Movies")), "~/Movies");
        assert_eq!(display_path(Path::new("/tmp")), "/tmp");
        assert_eq!(group_digits(1_234_567), "1,234,567");
        assert_eq!(group_digits(999), "999");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn skips_other_volumes_when_scanning_root() {
        let skip = skip_list(Path::new("/"));
        assert!(skip.contains(&PathBuf::from("/Volumes")));
        assert!(skip.contains(&PathBuf::from("/System/Volumes")));
        let inside = skip_list(Path::new("/Volumes/External"));
        assert!(!inside.contains(&PathBuf::from("/Volumes")));
    }

    #[test]
    fn classifies_icons() {
        assert_eq!(icon_for(ItemKind::File, "Movie.MOV"), "file-video-camera");
        assert_eq!(icon_for(ItemKind::File, "x.dmg"), "file-archive");
        assert_eq!(icon_for(ItemKind::Folder, "x"), "folder");
        assert_eq!(icon_for(ItemKind::App, "Xcode.app"), "app-window");
    }

    /// A junction `link` → `target` (no admin rights needed); false when it
    /// could not be made.
    #[cfg(windows)]
    fn junction(link: &Path, target: &Path) -> bool {
        std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .output()
            .is_ok_and(|o| o.status.success())
    }

    #[cfg(windows)]
    #[test]
    fn lists_children_on_windows_without_entering_junctions() {
        let t = TempDir::new("list-win");
        t.file("big/a.bin", 300_000);
        t.file("big/nested/deeper/b.bin", 200_000);
        t.file("small/c.txt", 10_000);
        t.file("loose.mov", 100_000);
        // Bundle names mean nothing on Windows.
        t.file("Tool.app/Contents/MacOS/tool", 50_000);
        let made_link = junction(&t.0.join("link"), &t.0.join("big"));

        let listing = list_children(&t.0, &Cancel::new(), &noop).unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(&names[..4], ["big", "loose.mov", "Tool.app", "small"]);

        let big = &listing.entries[0];
        assert_eq!(
            (big.kind, big.size, big.files),
            (ItemKind::Folder, 500_000, 2)
        );
        assert_eq!(listing.entries[1].kind, ItemKind::File);
        assert_eq!(listing.entries[2].kind, ItemKind::Folder);
        assert!(listing.entries[2].kind.is_drillable());

        if made_link {
            let link = listing.entries.iter().find(|e| e.name == "link").unwrap();
            assert_eq!(link.kind, ItemKind::Symlink);
            assert_eq!(link.kind.label(), "Link");
            assert_eq!(link.size, 0, "junctions are not followed");
            assert!(!link.mounted);
            assert_eq!(
                link.alias_of.as_deref().map(display_path),
                Some(display_path(&t.0.join("big")))
            );
            assert_eq!(listing.total, 660_000);
        }
        assert_eq!(
            listing.total,
            listing.entries.iter().map(|e| e.size).sum::<u64>()
        );
    }

    #[cfg(windows)]
    #[test]
    fn finds_large_files_on_windows() {
        let t = TempDir::new("large-win");
        t.file("a/huge.dmg", 400_000);
        t.file("a/b/medium.mov", 250_000);
        t.file("tiny.txt", 1_000);
        // Not a package on Windows: its files are judged one by one.
        t.file("Lib.photoslibrary/originals/x.heic", 150_000);
        t.file("Lib.photoslibrary/originals/y.heic", 150_000);

        let found = find_large_files(&t.0, 200_000, 10, &Cancel::new(), &|_| {}).unwrap();
        let names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["huge.dmg", "medium.mov"]);
        assert_eq!(found[0].size, 400_000);
        assert!(found.iter().all(|f| f.protection.is_none()));

        let cancel = Cancel::new();
        cancel.cancel();
        assert_eq!(
            find_large_files(&t.0, 1, 10, &cancel, &|_| {}),
            Err(ScanError::Cancelled)
        );
    }

    #[cfg(windows)]
    #[test]
    fn hard_links_count_per_name_like_explorer_outside_the_windows_folder() {
        let t = TempDir::new("links-win");
        let one = t.file("a/data.bin", 400_000);
        std::fs::create_dir_all(t.0.join("a/inner")).unwrap();
        std::fs::hard_link(&one, t.0.join("a/inner/same.bin")).unwrap();
        t.file("c/other.bin", 100_000);

        // Explorer's folder "Size" adds every name: 400 KB + 400 KB + 100 KB.
        let listing = list_children(&t.0, &Cancel::new(), &noop).unwrap();
        assert_eq!(listing.total, 900_000);

        // Where links are tracked (the Windows folder), the second name adds nothing.
        let links = Links::new();
        let mut counted = Vec::new();
        for dir in ["a", "a/inner"] {
            for entry in std::fs::read_dir(t.0.join(dir)).unwrap().flatten() {
                let meta = entry.metadata().unwrap();
                if meta.is_file() {
                    let size = entry_allocated(&entry, &meta);
                    counted.push(links.counted_size(&meta, size, || link_key(&entry, &meta)));
                }
            }
        }
        counted.sort_unstable();
        assert_eq!(counted, [0, 400_000]);
    }

    #[cfg(windows)]
    #[test]
    fn compressed_files_use_their_allocation() {
        let t = TempDir::new("compact");
        let file = t.0.join("zeros.bin");
        std::fs::write(&file, vec![0u8; 4_000_000]).unwrap();
        let compressed = std::process::Command::new("compact")
            .args(["/c", "/q"])
            .arg(&file)
            .output()
            .is_ok_and(|o| o.status.success());
        let meta = std::fs::symlink_metadata(&file).unwrap();
        if !compressed || allocated(|| file.clone(), &meta) >= 4_000_000 {
            return; // not NTFS or compression unavailable
        }
        // 4 MB of zeros compress to almost nothing: not a large file.
        let found = find_large_files(&t.0, 1_000_000, 10, &Cancel::new(), &|_| {}).unwrap();
        assert!(found.is_empty(), "{found:?}");
        let listing = list_children(&t.0, &Cancel::new(), &noop).unwrap();
        assert!(listing.total < 1_000_000, "{}", listing.total);
    }

    #[cfg(windows)]
    #[test]
    fn breadcrumb_and_paths_on_windows() {
        let root = Path::new(r"C:\a\b");
        let trail = breadcrumb(root, Path::new(r"C:\a\b\c\d"));
        let labels: Vec<&str> = trail.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["b", "c", "d"]);
        assert_eq!(trail[2].1, PathBuf::from(r"C:\a\b\c\d"));
        assert_eq!(
            parent_within(root, Path::new(r"C:\a\b\c")),
            Some(PathBuf::from(r"C:\a\b"))
        );
        assert_eq!(parent_within(root, root), None);

        let drive = nera_core::paths::system_drive();
        let trail = breadcrumb(&drive, &drive.join("Users"));
        assert_eq!(trail[0].0, folder_name(&drive));
        assert_eq!(trail[1].0, "Users");
        assert_eq!(
            display_path(&home().join("Videos")),
            format!(r"{}\Videos", standardize(home()).display())
        );
        assert!(skip_list(&drive).is_empty());
        assert_eq!(
            classify(&drive, &std::fs::metadata(&drive).unwrap()),
            ItemKind::Folder
        );
    }

    /// Read-only smoke test against the real home folder (manual: `--ignored`).
    #[test]
    #[ignore]
    fn smoke_real_home_read_only() {
        let start = std::time::Instant::now();
        let listing = list_children(&home(), &Cancel::new(), &|_| {}).unwrap();
        eprintln!(
            "home: {} in {:?}, unreadable {}",
            nera_core::format::bytes(listing.total),
            start.elapsed(),
            listing.unreadable
        );
        for e in listing.entries.iter().take(8) {
            eprintln!(
                "  {:>10}  {:?} {}",
                nera_core::format::bytes(e.size),
                e.kind,
                e.name
            );
        }
        let start = std::time::Instant::now();
        let large = find_large_files(
            &home(),
            LARGE_FILE_THRESHOLD,
            LARGE_FILE_LIMIT,
            &Cancel::new(),
            &|_| {},
        )
        .unwrap();
        eprintln!("large: {} in {:?}", large.len(), start.elapsed());
        for f in large.iter().take(5) {
            eprintln!(
                "  {:>10}  {}",
                nera_core::format::bytes(f.size),
                display_path(&f.path)
            );
        }
        eprintln!("fda: {:?}", has_full_disk_access());
    }
}
