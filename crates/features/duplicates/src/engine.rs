//! Duplicate finder: port of `DuplicateFinderActor` / `DuplicateGroup`.
//!
//! Pipeline: walk the chosen folders and group regular files by size →
//! for same-size files hash the first and last 64 KB (blake3) → for partial
//! matches hash the whole file. Files that share a full hash are duplicates.
//!
//! Removal always goes to the Trash (these are user documents) and never
//! removes every copy of a group.

use nera_core::fs_util::Cancel;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

// Walking, file identity and default folders differ per platform; both files
// export the same names.
#[cfg(target_os = "macos")]
#[path = "engine_macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "engine_windows.rs"]
mod platform;
#[cfg(windows)]
pub use platform::is_cloud_only;
pub use platform::{clone_id, default_folders, shared_info};
use platform::{identify, index};

/// Bytes hashed at each end of a file in the partial-hash phase.
pub const PARTIAL_CHUNK: u64 = 64 * 1024;
const READ_BUF: usize = 1024 * 1024;
/// Directory extensions macOS treats as packages (documents/bundles). Their
/// internals are never compared file by file.
const PACKAGE_EXTENSIONS: &[&str] = &[
    "app",
    "appex",
    "bundle",
    "framework",
    "plugin",
    "kext",
    "prefpane",
    "qlgenerator",
    "mdimporter",
    "saver",
    "component",
    "xpc",
    "dsym",
    "photoslibrary",
    "photolibrary",
    "aplibrary",
    "musiclibrary",
    "tvlibrary",
    "imovielibrary",
    "fcpbundle",
    "logicx",
    "band",
    "lrlibrary",
    "lrdata",
    "pages",
    "numbers",
    "key",
    "rtfd",
    "xcodeproj",
    "xcworkspace",
    "xcarchive",
    "playground",
    "mlmodelc",
    "sparsebundle",
    "vmwarevm",
    "pvm",
    "utm",
    "docarchive",
    "scptd",
    "wdgt",
    "nib",
    "storyboardc",
];

// MARK: - Options

/// Minimum file size choices offered in the UI (decimal units, like Finder).
pub const MIN_SIZE_CHOICES: [u64; 4] = [100_000, 1_000_000, 10_000_000, 100_000_000];
pub const DEFAULT_MIN_SIZE: u64 = 1_000_000;

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub roots: Vec<PathBuf>,
    /// Files smaller than this are ignored (always at least 1 byte).
    pub min_size: u64,
    pub include_hidden: bool,
    /// Subtrees never scanned (user's excluded paths).
    pub excluded: Vec<PathBuf>,
}

/// Drop duplicates and folders nested inside another chosen folder.
pub fn normalize_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut std: Vec<PathBuf> = roots.iter().map(nera_core::paths::standardize).collect();
    std.sort();
    std.dedup();
    let mut out: Vec<PathBuf> = Vec::new();
    for p in std {
        if !out.iter().any(|o| p.starts_with(o)) {
            out.push(p);
        }
    }
    out
}

// MARK: - Results

/// Identity of the bytes on disk behind a file. Hard links share an inode and
/// APFS clones (Finder copies, `cp -c`) share extents; deleting one of several
/// files with the same `Family` frees nothing until the last one goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Family(pub(crate) u64, pub(crate) u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DuplicateFile {
    pub path: PathBuf,
    /// Logical size (what Finder shows as "Size").
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub family: Family,
    /// Allocated bytes on disk (`st_blocks * 512`).
    pub allocated: u64,
    /// Deleting this file frees nothing however many copies go, because its
    /// data is also held by a file outside the scan (hard link or APFS clone).
    /// Such a file is the ideal copy to keep.
    pub pinned: bool,
}

impl DuplicateFile {
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Bytes freed when this file's data goes: its size, or less when the file
    /// is sparse or compressed and occupies fewer blocks than its length.
    pub fn cost(&self) -> u64 {
        self.size.min(self.allocated)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DuplicateGroup {
    /// Content hash (hex). Stable domain id for the group.
    pub id: String,
    /// Size of one copy.
    pub size: u64,
    /// Sorted by path.
    pub files: Vec<DuplicateFile>,
}

impl DuplicateGroup {
    /// Distinct copies of the data on disk (hard links and clones count once).
    pub fn families(&self) -> usize {
        self.files
            .iter()
            .map(|f| f.family)
            .collect::<HashSet<_>>()
            .len()
    }

    /// Per copy on disk: `(pinned, cost)`.
    fn copies(&self) -> HashMap<Family, (bool, u64)> {
        let mut m: HashMap<Family, (bool, u64)> = HashMap::new();
        for f in &self.files {
            let e = m.entry(f.family).or_insert((false, f.cost()));
            e.0 |= f.pinned;
            e.1 = e.1.max(f.cost());
        }
        m
    }

    /// True when some copy cannot be freed (its data lives on outside the scan).
    pub fn has_pinned(&self) -> bool {
        self.files.iter().any(|f| f.pinned)
    }

    /// Copies on disk that can be removed while one copy stays: every freeable
    /// copy when a pinned copy remains, otherwise all but one.
    pub fn removable_copies(&self) -> usize {
        let free = self.copies().values().filter(|(pinned, _)| !pinned).count();
        if self.has_pinned() {
            free
        } else {
            free.saturating_sub(1)
        }
    }

    /// Space freed by keeping exactly one copy: the size of every removable
    /// copy (`size × (copies on disk − 1)` for ordinary files). When all copies
    /// are freeable the most expensive one is the one kept, so this is a
    /// lower bound.
    pub fn wasted(&self) -> u64 {
        let copies = self.copies();
        let free = copies
            .values()
            .filter(|(pinned, _)| !pinned)
            .map(|(_, cost)| *cost);
        let sum: u64 = free.clone().sum();
        if self.has_pinned() {
            sum
        } else {
            sum - free.max().unwrap_or(0)
        }
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.files.iter().any(|f| f.path == path)
    }

    /// True when another file in the group shares `file`'s data on disk
    /// (hard link or clone inside the scan).
    pub fn shares_family(&self, file: &DuplicateFile) -> bool {
        self.files
            .iter()
            .any(|f| f.family == file.family && f.path != file.path)
    }

    /// True when `file` shares its data with another file, inside the group or outside the scan.
    pub fn shares_data(&self, file: &DuplicateFile) -> bool {
        file.pinned || self.shares_family(file)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Indexing,
    /// Partial (first/last 64 KB) hashes of same-size files.
    Comparing,
    /// Full hashes of partial matches.
    Verifying,
}

#[derive(Clone, Debug, Default)]
pub struct ScanProgress {
    pub phase: Phase,
    /// Files seen while indexing.
    pub files_seen: usize,
    /// Files hashed in the current phase.
    pub done: usize,
    /// Files to hash in the current phase.
    pub total: usize,
    /// File or folder currently being processed (display name).
    pub current: Option<String>,
}

impl ScanProgress {
    pub fn fraction(&self) -> Option<f64> {
        match self.phase {
            Phase::Indexing => None,
            _ if self.total == 0 => Some(0.0),
            _ => Some(self.done as f64 / self.total as f64),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ScanReport {
    /// Largest waste first.
    pub groups: Vec<DuplicateGroup>,
    /// Regular files at or above the minimum size.
    pub files_considered: usize,
    /// Chosen folders that were actually searched.
    pub folders_searched: usize,
    /// Sets of identical files that all share one copy of the data on disk
    /// (hard links, APFS clones): deleting any of them frees nothing.
    pub shared_sets: usize,
    /// Folders that could not be read (permission denied).
    pub denied: Vec<PathBuf>,
    /// Chosen folders that do not exist.
    pub missing: Vec<PathBuf>,
    /// Files that could not be read while hashing.
    pub unreadable: usize,
    pub cancelled: bool,
}

impl ScanReport {
    pub fn wasted(&self) -> u64 {
        self.groups.iter().map(DuplicateGroup::wasted).sum()
    }

    /// Extra copies: separate copies on disk that can go while one stays.
    pub fn extra_copies(&self) -> usize {
        self.groups
            .iter()
            .map(DuplicateGroup::removable_copies)
            .sum()
    }
}

// MARK: - Scan

#[derive(Clone, Debug)]
struct Candidate {
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
    dev: u64,
    ino: u64,
    nlink: u64,
    /// Allocated bytes (`st_blocks * 512`).
    allocated: u64,
    /// Metadata from the walk, for `fs_util::file_key` in `identify`.
    #[cfg(windows)]
    meta: std::fs::Metadata,
}

fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

/// True when `path` is a directory macOS shows as a single item (bundle/package).
pub fn is_package(path: &Path) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|e| PACKAGE_EXTENSIONS.contains(&e.as_str()))
}

/// Run the whole pipeline. Blocking; call from a background thread.
pub fn scan(
    opts: &ScanOptions,
    cancel: &Cancel,
    progress: &(dyn Fn(ScanProgress) + Sync),
) -> ScanReport {
    let mut report = ScanReport::default();
    let Some(size_groups) = index(opts, cancel, progress, &mut report) else {
        report.cancelled = true;
        return report;
    };

    // Phase 2: partial hashes of same-size files.
    let considered = report.files_considered;
    let candidates: Vec<Candidate> = size_groups
        .into_values()
        .filter(|g| g.len() > 1)
        .flatten()
        .collect();
    let total = candidates.len();
    let done = AtomicUsize::new(0);
    let unreadable = AtomicUsize::new(0);
    progress(ScanProgress {
        phase: Phase::Comparing,
        files_seen: considered,
        done: 0,
        total,
        current: None,
    });
    let partials: Vec<Option<(Candidate, blake3::Hash)>> = candidates
        .into_par_iter()
        .map(|mut c| {
            if cancel.is_cancelled() {
                return None;
            }
            // Windows reads hard-link identity here, only for same-size files.
            let hash = identify(&mut c).and_then(|()| partial_hash(&c.path, c.size));
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_multiple_of(32) || n == total {
                progress(ScanProgress {
                    phase: Phase::Comparing,
                    files_seen: considered,
                    done: n,
                    total,
                    current: Some(display_name(&c.path)),
                });
            }
            match hash {
                Ok(h) => Some((c, h)),
                Err(_) => {
                    unreadable.fetch_add(1, Ordering::Relaxed);
                    None
                }
            }
        })
        .collect();
    if cancel.is_cancelled() {
        report.cancelled = true;
        return report;
    }

    let mut by_partial: HashMap<(u64, blake3::Hash), Vec<Candidate>> = HashMap::new();
    for (c, h) in partials.into_iter().flatten() {
        by_partial.entry((c.size, h)).or_default().push(c);
    }

    // Small files were hashed in full already; only larger ones need phase 3.
    let mut groups: Vec<DuplicateGroup> = Vec::new();
    let mut shared_sets = 0usize;
    let mut to_verify: Vec<Candidate> = Vec::new();
    for ((size, hash), files) in by_partial {
        if files.len() < 2 {
            continue;
        }
        if size <= 2 * PARTIAL_CHUNK {
            push_group(&mut groups, &mut shared_sets, make_group(hash, size, files));
        } else {
            to_verify.extend(files);
        }
    }

    // Phase 3: full hashes.
    let total = to_verify.len();
    let done = AtomicUsize::new(0);
    progress(ScanProgress {
        phase: Phase::Verifying,
        files_seen: considered,
        done: 0,
        total,
        current: None,
    });
    let fulls: Vec<Option<(Candidate, blake3::Hash)>> = to_verify
        .into_par_iter()
        .map(|c| {
            if cancel.is_cancelled() {
                return None;
            }
            let hash = full_hash(&c.path, cancel);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            progress(ScanProgress {
                phase: Phase::Verifying,
                files_seen: considered,
                done: n,
                total,
                current: Some(display_name(&c.path)),
            });
            match hash {
                Ok(h) => Some((c, h)),
                Err(_) => {
                    if !cancel.is_cancelled() {
                        unreadable.fetch_add(1, Ordering::Relaxed);
                    }
                    None
                }
            }
        })
        .collect();
    if cancel.is_cancelled() {
        report.cancelled = true;
        return report;
    }

    let mut by_full: HashMap<(u64, blake3::Hash), Vec<Candidate>> = HashMap::new();
    for (c, h) in fulls.into_iter().flatten() {
        by_full.entry((c.size, h)).or_default().push(c);
    }
    for ((size, hash), files) in by_full {
        if files.len() > 1 {
            push_group(&mut groups, &mut shared_sets, make_group(hash, size, files));
        }
    }

    groups.sort_by(|a, b| b.wasted().cmp(&a.wasted()).then_with(|| a.id.cmp(&b.id)));
    report.groups = groups;
    report.shared_sets = shared_sets;
    report.unreadable = unreadable.into_inner();
    report.denied.sort();
    report.denied.dedup();
    report
}

fn make_group(hash: blake3::Hash, size: u64, files: Vec<Candidate>) -> DuplicateGroup {
    // Hard links of one inode seen inside the group.
    let mut links: HashMap<(u64, u64), u64> = HashMap::new();
    for c in &files {
        if c.nlink > 1 {
            *links.entry((c.dev, c.ino)).or_insert(0) += 1;
        }
    }
    let info: Vec<Option<(u64, u64)>> = files.iter().map(|c| shared_info(&c.path)).collect();
    // Copies of one clone family seen inside the group.
    let mut clones: HashMap<(u64, u64), u64> = HashMap::new();
    for (c, i) in files.iter().zip(&info) {
        if let (Some((_, id)), true) = (i, c.nlink <= 1) {
            if *id != 0 {
                *clones.entry((c.dev, *id)).or_insert(0) += 1;
            }
        }
    }
    let mut out: Vec<DuplicateFile> = Vec::new();
    for (c, i) in files.into_iter().zip(info) {
        let (family, pinned) = if c.nlink > 1 {
            // Deleting a hard link frees nothing unless every link goes.
            let outside = links.get(&(c.dev, c.ino)).copied().unwrap_or(0) < c.nlink;
            (Family(c.dev, c.ino | FAMILY_INODE), outside)
        } else {
            match i {
                Some((_, id)) if id != 0 && clones.get(&(c.dev, id)).copied().unwrap_or(0) > 1 => {
                    (Family(c.dev, id), false)
                }
                // Alone in its group: it must hold all of its blocks itself, or
                // it shares them with a file we cannot see (clone of a file
                // outside the scanned folders), and deleting it frees less.
                Some((private, _)) if private < c.allocated => {
                    (Family(c.dev, c.ino | FAMILY_INODE), true)
                }
                _ => (Family(c.dev, c.ino | FAMILY_INODE), false),
            }
        };
        out.push(DuplicateFile {
            path: c.path,
            size: c.size,
            modified: c.modified,
            family,
            allocated: c.allocated,
            pinned,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    DuplicateGroup {
        id: hash.to_hex().to_string(),
        size,
        files: out,
    }
}

/// Sets in which nothing can be freed (all copies share one copy of the data,
/// or every copy shares it with files outside the scan) are counted, not listed.
fn push_group(groups: &mut Vec<DuplicateGroup>, shared_sets: &mut usize, group: DuplicateGroup) {
    if group.removable_copies() >= 1 {
        groups.push(group);
    } else {
        *shared_sets += 1;
    }
}

const FAMILY_INODE: u64 = 1 << 63;

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// blake3 of the first and last [`PARTIAL_CHUNK`] bytes (the whole file when
/// it is no larger than two chunks).
pub fn partial_hash(path: &Path, size: u64) -> std::io::Result<blake3::Hash> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    if size <= 2 * PARTIAL_CHUNK {
        let mut buf = Vec::with_capacity(size as usize);
        file.read_to_end(&mut buf)?;
        hasher.update(&buf);
    } else {
        let mut buf = vec![0u8; PARTIAL_CHUNK as usize];
        file.read_exact(&mut buf)?;
        hasher.update(&buf);
        file.seek(SeekFrom::Start(size - PARTIAL_CHUNK))?;
        file.read_exact(&mut buf)?;
        hasher.update(&buf);
    }
    Ok(hasher.finalize())
}

/// blake3 of the whole file, read in 1 MB chunks; aborts when cancelled.
pub fn full_hash(path: &Path, cancel: &Cancel) -> std::io::Result<blake3::Hash> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; READ_BUF];
    loop {
        if cancel.is_cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "cancelled",
            ));
        }
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

// MARK: - Selection

/// Which copy of each group is kept when auto-selecting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeepRule {
    #[default]
    Newest,
    Oldest,
    ShortestPath,
}

impl KeepRule {
    pub const ALL: [KeepRule; 3] = [KeepRule::Newest, KeepRule::Oldest, KeepRule::ShortestPath];

    pub fn label(self) -> &'static str {
        match self {
            KeepRule::Newest => "Newest",
            KeepRule::Oldest => "Oldest",
            KeepRule::ShortestPath => "Shortest path",
        }
    }
}

/// The copy `rule` keeps. Ties fall back to the shortest path, then path order.
/// A copy whose data also lives outside the scan is always kept in preference:
/// deleting it would free nothing, and keeping it lets every other copy go.
pub fn keeper(group: &DuplicateGroup, rule: KeepRule) -> Option<&DuplicateFile> {
    let path_len = |f: &DuplicateFile| f.path.as_os_str().len();
    let mtime = |f: &DuplicateFile| f.modified.unwrap_or(SystemTime::UNIX_EPOCH);
    let prefer_pinned = group.has_pinned();
    group
        .files
        .iter()
        .filter(|f| f.pinned || !prefer_pinned)
        .min_by(|a, b| {
            let primary = match rule {
                KeepRule::Newest => mtime(b).cmp(&mtime(a)),
                KeepRule::Oldest => mtime(a).cmp(&mtime(b)),
                KeepRule::ShortestPath => path_len(a)
                    .cmp(&path_len(b))
                    .then_with(|| mtime(a).cmp(&mtime(b))),
            };
            primary
                .then_with(|| path_len(a).cmp(&path_len(b)))
                .then_with(|| a.path.cmp(&b.path))
        })
}

/// Paths to mark for removal in `group`: every copy except the keeper, the
/// files that share the keeper's data and the files whose data lives on outside
/// the scan (removing those frees nothing).
pub fn auto_select_group(group: &DuplicateGroup, rule: KeepRule) -> Vec<PathBuf> {
    let Some(keep) = keeper(group, rule) else {
        return Vec::new();
    };
    group
        .files
        .iter()
        .filter(|f| f.family != keep.family && !f.pinned)
        .map(|f| f.path.clone())
        .collect()
}

/// Selection for every group under `rule`.
pub fn auto_select(groups: &[DuplicateGroup], rule: KeepRule) -> HashSet<PathBuf> {
    groups
        .iter()
        .flat_map(|g| auto_select_group(g, rule))
        .collect()
}

/// True when selecting `path` still leaves at least one copy in its group.
pub fn can_select(group: &DuplicateGroup, selected: &HashSet<PathBuf>, path: &Path) -> bool {
    group
        .files
        .iter()
        .any(|f| f.path != path && !selected.contains(&f.path))
}

/// Selected copies in `group`.
pub fn selected_in(group: &DuplicateGroup, selected: &HashSet<PathBuf>) -> usize {
    group
        .files
        .iter()
        .filter(|f| selected.contains(&f.path))
        .count()
}

/// Space freed once the selected files are gone: one copy's size for every
/// copy on disk whose files are all selected.
pub fn selected_size(groups: &[DuplicateGroup], selected: &HashSet<PathBuf>) -> u64 {
    groups
        .iter()
        .map(|g| freed_by(g, |p| selected.contains(p)))
        .sum()
}

/// True when some selected file shares its data with another file of its set
/// (hard link or clone): then the freed size also depends on files outside the
/// scan, and is an upper bound.
pub fn selection_is_upper_bound(groups: &[DuplicateGroup], selected: &HashSet<PathBuf>) -> bool {
    groups.iter().any(|g| {
        g.files
            .iter()
            .any(|f| selected.contains(&f.path) && !f.pinned && g.shares_family(f))
    })
}

/// Cost of every freeable copy on disk whose files are all removed, for one group.
fn freed_by(group: &DuplicateGroup, removed: impl Fn(&Path) -> bool) -> u64 {
    let mut state: HashMap<Family, bool> = HashMap::new();
    for f in &group.files {
        let all = state.entry(f.family).or_insert(true);
        *all &= removed(&f.path);
    }
    let copies = group.copies();
    state
        .iter()
        .filter(|(fam, all)| **all && copies.get(fam).is_some_and(|(pinned, _)| !pinned))
        .map(|(fam, _)| copies[fam].1)
        .sum()
}

// MARK: - Removal

#[derive(Clone, Debug, Default)]
pub struct TrashOutcome {
    pub trashed: Vec<PathBuf>,
    /// Space that frees up once the Trash is emptied. An upper bound: a copy
    /// that also has a hard link or clone outside the scanned folders keeps
    /// its data alive.
    pub freed: u64,
    pub failed: Vec<(PathBuf, String)>,
}

/// Verify a file still matches what the scan saw.
fn unchanged(file: &DuplicateFile) -> bool {
    match std::fs::symlink_metadata(&file.path) {
        Ok(m) => {
            m.is_file()
                && m.len() == file.size
                && (file.modified.is_none() || m.modified().ok() == file.modified)
        }
        Err(_) => false,
    }
}

/// Move the selected copies to the Trash via `trash` (normally
/// `nera_core::fs_util::trash_item`). Per group, at least one unselected copy
/// must still be on disk unchanged, otherwise the whole group is skipped.
pub fn trash_selected(
    groups: &[DuplicateGroup],
    selected: &HashSet<PathBuf>,
    cancel: &Cancel,
    progress: &dyn Fn(usize, usize),
    trash: &dyn Fn(&Path) -> std::io::Result<()>,
) -> TrashOutcome {
    let mut out = TrashOutcome::default();
    let total: usize = groups.iter().map(|g| selected_in(g, selected)).sum();
    let mut done = 0usize;
    for group in groups {
        let picked: Vec<&DuplicateFile> = group
            .files
            .iter()
            .filter(|f| selected.contains(&f.path))
            .collect();
        if picked.is_empty() {
            continue;
        }
        let kept_ok = group
            .files
            .iter()
            .any(|f| !selected.contains(&f.path) && unchanged(f));
        if !kept_ok {
            for f in &picked {
                out.failed
                    .push((f.path.clone(), "No other copy remains, kept".into()));
            }
            done += picked.len();
            continue;
        }
        let mut moved: HashSet<PathBuf> = HashSet::new();
        for f in picked {
            if cancel.is_cancelled() {
                out.freed += freed_by(group, |p| moved.contains(p));
                return out;
            }
            done += 1;
            progress(done, total);
            if !unchanged(f) {
                out.failed
                    .push((f.path.clone(), "Changed since the scan, kept".into()));
                continue;
            }
            match trash(&f.path) {
                Ok(()) => {
                    moved.insert(f.path.clone());
                    out.trashed.push(f.path.clone());
                }
                Err(e) => out.failed.push((f.path.clone(), e.to_string())),
            }
        }
        out.freed += freed_by(group, |p| moved.contains(p));
    }
    out
}

/// Remove trashed files from the results; groups with nothing left to free
/// (a single copy on disk, hard links / clones of it) vanish.
pub fn apply_removal(groups: &mut Vec<DuplicateGroup>, removed: &[PathBuf]) {
    let removed: HashSet<&PathBuf> = removed.iter().collect();
    for g in groups.iter_mut() {
        g.files.retain(|f| !removed.contains(&f.path));
    }
    groups.retain(|g| g.removable_copies() >= 1);
}

// MARK: - Preferences

/// Remembered choices for this screen (own file, not the shared settings).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub folders: Vec<PathBuf>,
    pub min_size: u64,
    pub keep_rule: KeepRule,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            folders: default_folders(),
            min_size: DEFAULT_MIN_SIZE,
            keep_rule: KeepRule::Newest,
        }
    }
}

impl Prefs {
    pub fn file_path() -> PathBuf {
        nera_core::paths::app_support_dir().join("duplicates.json")
    }

    pub fn load_from(path: &Path) -> Self {
        let mut p: Prefs = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        p.folders.retain(|f| f.is_dir());
        if !MIN_SIZE_CHOICES.contains(&p.min_size) {
            p.min_size = DEFAULT_MIN_SIZE;
        }
        p
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        nera_core::fs_util::write_atomic(path, &serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("nera-dup-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
        fn write(&self, rel: &str, data: &[u8]) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, data).unwrap();
            p
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn opts(root: &Path, min: u64) -> ScanOptions {
        ScanOptions {
            roots: vec![root.to_path_buf()],
            min_size: min,
            include_hidden: false,
            excluded: vec![],
        }
    }

    fn run(o: &ScanOptions) -> ScanReport {
        scan(o, &Cancel::new(), &|_| {})
    }

    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    #[test]
    fn finds_small_and_large_duplicates() {
        let t = TempDir::new("basic");
        let small = pattern(5_000, 1);
        t.write("a/one.txt", &small);
        t.write("b/two.txt", &small);
        t.write("unique.txt", &pattern(5_000, 2));
        let big = pattern(400_000, 3);
        t.write("big1.bin", &big);
        t.write("x/big2.bin", &big);
        t.write("y/big3.bin", &big);

        let r = run(&opts(&t.0, 1_000));
        assert!(!r.cancelled);
        assert_eq!(r.groups.len(), 2);
        assert_eq!(r.groups[0].size, 400_000);
        assert_eq!(r.groups[0].files.len(), 3);
        assert_eq!(r.groups[1].files.len(), 2);
        assert_eq!(r.wasted(), 800_000 + 5_000);
        assert_eq!(r.extra_copies(), 3);
        assert_eq!(r.files_considered, 6);
    }

    #[test]
    fn same_ends_different_middle_are_not_duplicates() {
        let t = TempDir::new("middle");
        let a = pattern(300_000, 7);
        let mut b = a.clone();
        b[150_000] ^= 0xff; // differs only outside the partial-hash windows
        t.write("a.bin", &a);
        t.write("b.bin", &b);
        assert_eq!(
            partial_hash(&t.0.join("a.bin"), 300_000).unwrap(),
            partial_hash(&t.0.join("b.bin"), 300_000).unwrap()
        );
        assert!(run(&opts(&t.0, 1)).groups.is_empty());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn respects_min_size_hidden_packages_and_symlinks() {
        let t = TempDir::new("filters");
        let data = pattern(2_000, 9);
        t.write("tiny1", &data[..100]);
        t.write("tiny2", &data[..100]);
        let orig = t.write("orig.dat", &data);
        t.write(".hidden/copy.dat", &data);
        t.write("Some.app/Contents/copy.dat", &data);
        t.write("Lib.photoslibrary/copy.dat", &data);
        t.write("zero1", b"");
        t.write("zero2", b"");
        std::os::unix::fs::symlink(&orig, t.0.join("link.dat")).unwrap();

        let r = run(&opts(&t.0, 1_000));
        assert!(r.groups.is_empty(), "{:?}", r.groups);
        assert_eq!(r.files_considered, 1); // orig.dat only: the rest is hidden, packaged, a symlink or too small

        let mut o = opts(&t.0, 1_000);
        o.include_hidden = true;
        let r = run(&o);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].files.len(), 2);
        assert!(r.groups[0]
            .files
            .iter()
            .any(|f| f.path.ends_with(".hidden/copy.dat")));
        assert_eq!(r.folders_searched, 1);

        // Trash folders are never scanned, even with hidden files on.
        t.write(".Trash/copy.dat", &data);
        assert_eq!(run(&o).groups[0].files.len(), 2);

        // Excluded subtree.
        o.excluded = vec![t.0.join(".hidden")];
        assert!(run(&o).groups.is_empty());

        // Tiny files appear once the threshold allows them; empty files never do.
        let r = run(&opts(&t.0, 50));
        assert!(r.groups.iter().any(|g| g.size == 100));
        let r = run(&opts(&t.0, 0));
        assert!(r.groups.iter().all(|g| g.size > 0));
    }

    #[test]
    fn hard_links_and_clones_share_space_and_are_not_reclaimable() {
        let t = TempDir::new("shared");
        let data = pattern(300_000, 11);
        let a = t.write("a.bin", &data);
        std::fs::hard_link(&a, t.0.join("a-hard.bin")).unwrap();
        // Only hard links of one file: identical, but nothing to reclaim.
        let r = run(&opts(&t.0, 1));
        assert!(r.groups.is_empty());
        assert_eq!(r.shared_sets, 1);
        assert_eq!(r.wasted(), 0);

        // A real second copy: 2 copies on disk, 3 files.
        t.write("sub/b.bin", &data);
        let r = run(&opts(&t.0, 1));
        assert_eq!(r.groups.len(), 1);
        let g = &r.groups[0];
        assert_eq!(g.files.len(), 3);
        assert_eq!(g.families(), 2);
        assert_eq!(g.wasted(), 300_000);
        assert_eq!(r.extra_copies(), 1);
        assert!(
            g.shares_data(&g.files[0])
                && !g.shares_data(
                    g.files
                        .iter()
                        .find(|f| f.path.ends_with("sub/b.bin"))
                        .unwrap()
                )
        );

        // Keeping a hard link never selects its twin; the independent copy goes.
        let keep_a = auto_select_group(g, KeepRule::ShortestPath);
        assert_eq!(keep_a, vec![t.0.join("sub/b.bin")]);
        // Keeping the independent copy selects both links, which together free one copy.
        let mut g2 = g.clone();
        for f in &mut g2.files {
            f.modified = Some(if f.path.ends_with("sub/b.bin") {
                SystemTime::now()
            } else {
                SystemTime::UNIX_EPOCH
            });
        }
        let picks = auto_select_group(&g2, KeepRule::Newest);
        assert_eq!(picks.len(), 2);
        let sel: HashSet<PathBuf> = picks.into_iter().collect();
        assert_eq!(selected_size(std::slice::from_ref(&g2), &sel), 300_000);
        // Only one link selected: nothing is freed.
        let one: HashSet<PathBuf> = [t.0.join("a.bin")].into_iter().collect();
        assert_eq!(selected_size(std::slice::from_ref(&g2), &one), 0);
    }

    #[test]
    fn data_shared_with_files_outside_the_scan_is_kept_never_freed() {
        let t = TempDir::new("outside");
        let data = pattern(300_000, 17);
        // Hard link whose other name is outside the scanned folder: deleting it
        // frees nothing, so it is the copy to keep and the plain copy can go.
        let a = t.write("scan/a.bin", &data);
        std::fs::hard_link(&a, t.0.join("elsewhere-link.bin")).unwrap();
        let copy = t.write("scan/copy.bin", &data);
        let r = run(&opts(&t.0.join("scan"), 1));
        assert_eq!(r.shared_sets, 0);
        assert_eq!(r.groups.len(), 1);
        let g = &r.groups[0];
        assert_eq!(g.files.len(), 2);
        assert!(g.files.iter().find(|f| f.path == a).unwrap().pinned);
        assert!(!g.files.iter().find(|f| f.path == copy).unwrap().pinned);
        assert_eq!(
            (g.wasted(), g.removable_copies(), r.extra_copies()),
            (300_000, 1, 1)
        );
        for rule in KeepRule::ALL {
            // The pinned copy is kept whatever the rule says; only the plain copy is picked.
            assert_eq!(keeper(g, rule).unwrap().path, a);
            assert_eq!(auto_select_group(g, rule), vec![copy.clone()]);
        }
        let sel: HashSet<PathBuf> = [copy.clone()].into_iter().collect();
        assert_eq!(selected_size(std::slice::from_ref(g), &sel), 300_000);
        // Removing the pinned copy instead frees nothing.
        let pin: HashSet<PathBuf> = [a.clone()].into_iter().collect();
        assert_eq!(selected_size(std::slice::from_ref(g), &pin), 0);
        assert!(g.shares_data(g.files.iter().find(|f| f.path == a).unwrap()));

        // Two copies that both live on outside the scan: nothing to free at all.
        std::fs::remove_file(&copy).unwrap();
        std::fs::hard_link(t.0.join("elsewhere-link.bin"), t.0.join("scan/b.bin")).unwrap();
        let r = run(&opts(&t.0.join("scan"), 1));
        // a.bin and b.bin are both links of one inode with a third link outside.
        assert!(r.groups.is_empty(), "{:?}", r.groups);
        assert_eq!(r.shared_sets, 1);

        // Finder-style clone of a file outside the scanned folder.
        let orig = t.write("src/orig.bin", &pattern(300_000, 19));
        std::fs::create_dir_all(t.0.join("scan2")).unwrap();
        let cloned = std::process::Command::new("/bin/cp")
            .arg("-c")
            .arg(&orig)
            .arg(t.0.join("scan2/x.bin"))
            .status()
            .is_ok_and(|s| s.success());
        if !cloned || shared_info(&t.0.join("scan2/x.bin")).is_none_or(|(private, _)| private != 0)
        {
            return; // not APFS
        }
        // Alone with its clone outside: nothing to free.
        t.write("scan2/y.bin", &pattern(300_000, 19));
        let r = run(&opts(&t.0.join("scan2"), 1));
        assert_eq!(r.groups.len(), 1);
        let g = &r.groups[0];
        assert!(
            g.files
                .iter()
                .find(|f| f.path.ends_with("x.bin"))
                .unwrap()
                .pinned
        );
        assert_eq!(g.wasted(), 300_000);
        assert_eq!(
            auto_select_group(g, KeepRule::Newest),
            vec![t.0.join("scan2/y.bin")]
        );
        std::fs::remove_file(t.0.join("scan2/y.bin")).unwrap();
        let r = run(&opts(&t.0.join("scan2"), 1));
        assert!(r.groups.is_empty());
        assert_eq!(r.shared_sets, 0); // a single file is not a set
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn min_size_boundary_and_sparse_files() {
        let t = TempDir::new("min");
        let d = pattern(10_000, 3);
        t.write("a", &d);
        t.write("b", &d);
        t.write("c", &d[..9_999]);
        t.write("d", &d[..9_999]);
        let r = run(&opts(&t.0, 10_000));
        assert_eq!(r.files_considered, 2); // exactly min is in, min - 1 is out
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].size, 10_000);
        let r = run(&opts(&t.0, 10_001));
        assert!(r.groups.is_empty() && r.files_considered == 0);
        let r = run(&opts(&t.0, 9_999));
        assert_eq!((r.groups.len(), r.files_considered), (2, 4));

        // A sparse file occupies fewer blocks than its length: only what it
        // really holds is claimed as reclaimable.
        let sparse = |name: &str| {
            let p = t.0.join(name);
            let f = std::fs::File::create(&p).unwrap();
            f.set_len(5_000_000).unwrap();
            p
        };
        sparse("s1");
        sparse("s2");
        let r = run(&opts(&t.0, 4_000_000));
        let g = &r.groups[0];
        let alloc = g.files[0].allocated;
        assert!(alloc < 5_000_000, "{alloc}");
        assert_eq!(g.size, 5_000_000);
        assert_eq!(g.wasted(), alloc);
    }

    #[test]
    fn freed_space_counts_a_family_only_when_every_file_goes() {
        let fam = |n: u64| Family(1, n);
        let file = |p: &str, f: u64| DuplicateFile {
            path: PathBuf::from(p),
            size: 1000,
            modified: None,
            family: fam(f),
            allocated: 4096,
            pinned: false,
        };
        let g = DuplicateGroup {
            id: "g".into(),
            size: 1000,
            files: vec![file("/a", 1), file("/b", 1), file("/c", 2), file("/d", 3)],
        };
        let sel = |ps: &[&str]| -> HashSet<PathBuf> { ps.iter().map(PathBuf::from).collect() };
        let one = std::slice::from_ref(&g);
        assert_eq!(selected_size(one, &sel(&["/a"])), 0);
        assert_eq!(selected_size(one, &sel(&["/a", "/b"])), 1000);
        assert_eq!(selected_size(one, &sel(&["/a", "/b", "/c"])), 2000);
        assert!(selection_is_upper_bound(one, &sel(&["/a", "/b"])));
        assert!(!selection_is_upper_bound(one, &sel(&["/c", "/d"])));
        // Same numbers through the Trash path.
        let calls = Mutex::new(Vec::new());
        let ok = |p: &Path| -> std::io::Result<()> {
            calls.lock().unwrap().push(p.to_path_buf());
            Ok(())
        };
        // Files do not exist, so `unchanged` rejects them: use real ones.
        let t = TempDir::new("freed");
        let d = pattern(4_000, 2);
        let paths: Vec<PathBuf> = ["x1", "x2", "y", "z"]
            .iter()
            .map(|n| t.write(n, &d))
            .collect();
        let mk = |p: &PathBuf, f: u64| {
            let m = std::fs::metadata(p).unwrap();
            DuplicateFile {
                path: p.clone(),
                size: 4_000,
                modified: m.modified().ok(),
                family: fam(f),
                allocated: 4_096,
                pinned: false,
            }
        };
        let real = DuplicateGroup {
            id: "r".into(),
            size: 4_000,
            files: vec![
                mk(&paths[0], 1),
                mk(&paths[1], 1),
                mk(&paths[2], 2),
                mk(&paths[3], 3),
            ],
        };
        let sel: HashSet<PathBuf> = [paths[0].clone(), paths[1].clone(), paths[2].clone()]
            .into_iter()
            .collect();
        let out = trash_selected(
            std::slice::from_ref(&real),
            &sel,
            &Cancel::new(),
            &|_, _| {},
            &ok,
        );
        assert_eq!(out.trashed.len(), 3);
        assert_eq!(out.freed, 8_000); // x1+x2 (one data copy) and y
        let only_one: HashSet<PathBuf> = [paths[0].clone()].into_iter().collect();
        let out = trash_selected(
            std::slice::from_ref(&real),
            &only_one,
            &Cancel::new(),
            &|_, _| {},
            &ok,
        );
        assert_eq!((out.trashed.len(), out.freed), (1, 0));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn apfs_clones_are_detected_and_not_counted_as_extra_space() {
        let t = TempDir::new("clone");
        let data = pattern(300_000, 13);
        let a = t.write("a.bin", &data);
        let cloned = std::process::Command::new("/bin/cp")
            .arg("-c")
            .arg(&a)
            .arg(t.0.join("b.bin"))
            .status()
            .is_ok_and(|s| s.success());
        let (Some(id_a), true) = (clone_id(&a), cloned) else {
            return;
        }; // not APFS: nothing to check
        assert_eq!(clone_id(&t.0.join("b.bin")), Some(id_a));
        t.write("c.bin", &data);
        assert_ne!(clone_id(&t.0.join("c.bin")), Some(id_a));

        let r = run(&opts(&t.0, 1));
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].files.len(), 3);
        assert_eq!(r.groups[0].wasted(), 300_000);

        // Clones of one file only: listed nowhere, counted as shared.
        std::fs::remove_file(t.0.join("c.bin")).unwrap();
        let r = run(&opts(&t.0, 1));
        assert!(r.groups.is_empty());
        assert_eq!(r.shared_sets, 1);
    }

    #[test]
    fn overlapping_roots_count_files_once() {
        let t = TempDir::new("overlap");
        let d = pattern(3_000, 4);
        t.write("sub/a", &d);
        t.write("b", &d);
        let o = ScanOptions {
            roots: vec![t.0.clone(), t.0.join("sub"), t.0.join("missing")],
            min_size: 1,
            include_hidden: false,
            excluded: vec![],
        };
        let r = run(&o);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].files.len(), 2);
        assert_eq!(normalize_roots(&o.roots).len(), 1);
        let r = run(&ScanOptions {
            roots: vec![t.0.join("missing")],
            ..o
        });
        assert_eq!(r.missing.len(), 1);
    }

    #[test]
    fn cancel_stops_the_scan() {
        let t = TempDir::new("cancel");
        t.write("a", &pattern(2_000, 1));
        t.write("b", &pattern(2_000, 1));
        let cancel = Cancel::new();
        cancel.cancel();
        let r = scan(&opts(&t.0, 1), &cancel, &|_| {});
        assert!(r.cancelled && r.groups.is_empty());
    }

    #[test]
    fn reports_progress_phases() {
        let t = TempDir::new("progress");
        let big = pattern(300_000, 5);
        t.write("a", &big);
        t.write("b", &big);
        let phases = Mutex::new(Vec::new());
        scan(&opts(&t.0, 1), &Cancel::new(), &|p| {
            phases.lock().unwrap().push((p.phase, p.done, p.total))
        });
        let phases = phases.into_inner().unwrap();
        assert!(phases.contains(&(Phase::Comparing, 2, 2)));
        assert!(phases.contains(&(Phase::Verifying, 2, 2)));
    }

    fn file(path: &str, secs: u64) -> DuplicateFile {
        DuplicateFile {
            path: PathBuf::from(path),
            size: 10,
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
            family: Family(1, path.len() as u64 + 100 * secs),
            allocated: 4_096,
            pinned: false,
        }
    }

    fn group() -> DuplicateGroup {
        DuplicateGroup {
            id: "g".into(),
            size: 10,
            files: vec![
                file("/a/very/long/path/x", 300),
                file("/b/x", 100),
                file("/c/dir/x", 200),
            ],
        }
    }

    #[test]
    fn keep_rules_pick_the_right_copy() {
        let g = group();
        assert_eq!(
            keeper(&g, KeepRule::Newest).unwrap().path,
            PathBuf::from("/a/very/long/path/x")
        );
        assert_eq!(
            keeper(&g, KeepRule::Oldest).unwrap().path,
            PathBuf::from("/b/x")
        );
        assert_eq!(
            keeper(&g, KeepRule::ShortestPath).unwrap().path,
            PathBuf::from("/b/x")
        );
        let sel = auto_select(std::slice::from_ref(&g), KeepRule::Newest);
        assert_eq!(sel.len(), 2);
        assert!(!sel.contains(Path::new("/a/very/long/path/x")));
        assert_eq!(selected_size(&[g], &sel), 20);
    }

    #[test]
    fn never_selects_every_copy() {
        let g = group();
        let mut sel = HashSet::new();
        sel.insert(PathBuf::from("/b/x"));
        assert!(can_select(&g, &sel, Path::new("/c/dir/x")));
        sel.insert(PathBuf::from("/c/dir/x"));
        assert!(!can_select(&g, &sel, Path::new("/a/very/long/path/x")));
    }

    #[test]
    fn trash_keeps_one_copy_and_skips_changed_files() {
        let t = TempDir::new("trash");
        let d = pattern(4_000, 6);
        t.write("keep", &d);
        t.write("dup1", &d);
        t.write("dup2", &d);
        let r = run(&opts(&t.0, 1));
        let mut groups = r.groups.clone();
        let g = &groups[0];
        let keep = t.0.join("keep");

        // Simulated trash: records calls, never touches the real Trash.
        let calls = Mutex::new(Vec::new());
        let fake = |p: &Path| -> std::io::Result<()> {
            calls.lock().unwrap().push(p.to_path_buf());
            Ok(())
        };

        // Every copy selected: refused for the whole group.
        let all: HashSet<PathBuf> = g.files.iter().map(|f| f.path.clone()).collect();
        let out = trash_selected(&groups, &all, &Cancel::new(), &|_, _| {}, &fake);
        assert!(out.trashed.is_empty() && out.failed.len() == 3);
        assert!(calls.lock().unwrap().is_empty());

        // A file changed after the scan is kept.
        std::fs::write(t.0.join("dup2"), pattern(4_001, 6)).unwrap();
        let sel: HashSet<PathBuf> = [t.0.join("dup1"), t.0.join("dup2")].into_iter().collect();
        let out = trash_selected(&groups, &sel, &Cancel::new(), &|_, _| {}, &fake);
        assert_eq!(out.trashed, vec![t.0.join("dup1")]);
        assert_eq!(out.freed, 4_000);
        assert_eq!(out.failed.len(), 1);
        assert!(!calls.lock().unwrap().contains(&keep));

        apply_removal(&mut groups, &out.trashed);
        assert_eq!(groups[0].files.len(), 2);
        // Two files left that share their data are not duplicates any more.
        let mut shared = groups.clone();
        let fam = shared[0].files[0].family;
        shared[0].files[1].family = fam;
        apply_removal(&mut shared, &[]);
        assert!(shared.is_empty());
        apply_removal(&mut groups, &[t.0.join("dup2")]);
        assert!(groups.is_empty());
    }

    #[test]
    fn trash_skips_group_when_kept_copy_vanished() {
        let t = TempDir::new("vanish");
        let d = pattern(4_000, 8);
        t.write("a", &d);
        t.write("b", &d);
        let r = run(&opts(&t.0, 1));
        std::fs::remove_file(t.0.join("a")).unwrap();
        let sel: HashSet<PathBuf> = [t.0.join("b")].into_iter().collect();
        let out = trash_selected(&r.groups, &sel, &Cancel::new(), &|_, _| {}, &|_| {
            panic!("must not trash")
        });
        assert!(out.trashed.is_empty() && out.failed.len() == 1);
    }

    #[test]
    fn prefs_round_trip_and_sanitize() {
        let t = TempDir::new("prefs");
        let path = t.0.join("duplicates.json");
        let p = Prefs {
            folders: vec![t.0.clone(), t.0.join("gone")],
            min_size: 10_000_000,
            keep_rule: KeepRule::Oldest,
        };
        p.save_to(&path).unwrap();
        let back = Prefs::load_from(&path);
        assert_eq!(back.folders, vec![t.0.clone()]);
        assert_eq!(back.min_size, 10_000_000);
        assert_eq!(back.keep_rule, KeepRule::Oldest);
        std::fs::write(&path, r#"{"min_size": 7}"#).unwrap();
        assert_eq!(Prefs::load_from(&path).min_size, DEFAULT_MIN_SIZE);
    }

    /// Read-only smoke test on a real folder:
    /// `NERA_DUP_SCAN_DIR=~/.cargo/registry cargo test -p nera-duplicates -- --ignored`
    #[test]
    #[ignore]
    fn scan_real_folder_readonly() {
        let Some(dir) = std::env::var_os("NERA_DUP_SCAN_DIR") else {
            return;
        };
        let start = std::time::Instant::now();
        let r = scan(&opts(Path::new(&dir), 10_000), &Cancel::new(), &|_| {});
        eprintln!(
            "{} files, {} groups, {} wasted, {} denied, {} unreadable in {:?}",
            r.files_considered,
            r.groups.len(),
            nera_core::format::bytes(r.wasted()),
            r.denied.len(),
            r.unreadable,
            start.elapsed()
        );
        for g in r.groups.iter().take(3) {
            eprintln!("  {} x{} {:?}", g.size, g.files.len(), g.files[0].path);
        }
    }

    /// Windows walk rules and sizes. Hard links are covered by the shared
    /// tests above (`std::fs::hard_link` works on NTFS).
    #[cfg(windows)]
    mod win {
        use super::*;
        use std::os::windows::ffi::OsStrExt as _;
        use std::os::windows::fs::MetadataExt as _;
        use std::os::windows::io::AsRawHandle as _;

        const HIDDEN: u32 = 0x2;
        const OFFLINE: u32 = 0x1000;

        fn add_attributes(p: &Path, attrs: u32) {
            let wide: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
            // DIRECTORY (0x10) is not settable.
            let current = std::fs::symlink_metadata(p).unwrap().file_attributes() & !0x10;
            // SAFETY: NUL-terminated path.
            let ok = unsafe {
                windows_sys::Win32::Storage::FileSystem::SetFileAttributesW(
                    wide.as_ptr(),
                    current | attrs,
                )
            };
            assert_ne!(ok, 0, "SetFileAttributesW {p:?}");
        }

        fn make_sparse(f: &File) {
            const FSCTL_SET_SPARSE: u32 = 0x0009_00C4;
            let mut returned = 0u32;
            // SAFETY: valid open handle; no buffers.
            let ok = unsafe {
                windows_sys::Win32::System::IO::DeviceIoControl(
                    f.as_raw_handle(),
                    FSCTL_SET_SPARSE,
                    std::ptr::null(),
                    0,
                    std::ptr::null_mut(),
                    0,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            assert_ne!(ok, 0, "FSCTL_SET_SPARSE");
        }

        /// Directory junction (no privilege needed, unlike symlinks).
        fn junction(link: &Path, target: &Path) -> bool {
            std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .is_ok_and(|o| o.status.success())
        }

        #[test]
        fn walk_skips_hidden_cloud_only_links_and_recycle_bin() {
            let t = TempDir::new("winfilters");
            let data = pattern(2_000, 9);
            let root = t.0.join("scan");
            let orig = t.write("scan/orig.dat", &data);
            t.write("scan/tiny1", &data[..100]);
            t.write("scan/tiny2", &data[..100]);
            t.write("scan/.dot/copy.dat", &data);
            t.write("scan/attr/copy.dat", &data);
            add_attributes(&root.join("attr"), HIDDEN);
            add_attributes(&t.write("scan/hidden-file.dat", &data), HIDDEN);
            t.write("scan/$Recycle.Bin/S-1-5-21/copy.dat", &data);
            // Stand-in for an online-only OneDrive file (its attributes cannot be set by hand).
            add_attributes(&t.write("scan/cloud/offline.dat", &data), OFFLINE);
            // Bundles are ordinary folders on Windows.
            t.write("scan/Some.app/Contents/copy.dat", &data);
            t.write("outside/copy.dat", &data);
            assert!(junction(&root.join("jn"), &t.0.join("outside")));
            assert!(std::fs::symlink_metadata(root.join("jn"))
                .unwrap()
                .file_type()
                .is_symlink());
            // Needs Developer Mode or admin rights; skipped files either way.
            let _ = std::os::windows::fs::symlink_file(&orig, root.join("link.dat"));

            let r = run(&opts(&root, 1_000));
            assert_eq!(r.files_considered, 2, "{:?}", r.groups); // orig + Some.app copy
            assert_eq!(r.groups.len(), 1);
            assert_eq!(r.groups[0].files.len(), 2);
            assert!(r.denied.is_empty() && r.unreadable == 0);

            let mut o = opts(&root, 1_000);
            o.include_hidden = true;
            let r = run(&o);
            assert_eq!(r.files_considered, 5); // + .dot, attr (hidden folder), hidden-file
            assert_eq!(r.groups.len(), 1);
            let names: Vec<String> = r.groups[0]
                .files
                .iter()
                .map(|f| f.path.strip_prefix(&root).unwrap().display().to_string())
                .collect();
            assert_eq!(
                names,
                [
                    r".dot\copy.dat",
                    r"Some.app\Contents\copy.dat",
                    r"attr\copy.dat",
                    "hidden-file.dat",
                    "orig.dat"
                ]
            );

            o.excluded = vec![root.join("attr")];
            assert_eq!(run(&o).files_considered, 4);

            // Tiny files appear once the threshold allows them.
            assert!(run(&opts(&root, 50)).groups.iter().any(|g| g.size == 100));
        }

        #[test]
        fn min_size_boundary_and_sparse_files() {
            let t = TempDir::new("winmin");
            let d = pattern(10_000, 3);
            t.write("a", &d);
            t.write("b", &d);
            t.write("c", &d[..9_999]);
            t.write("d", &d[..9_999]);
            let r = run(&opts(&t.0, 10_000));
            assert_eq!(r.files_considered, 2);
            assert_eq!(r.groups.len(), 1);
            let r = run(&opts(&t.0, 9_999));
            assert_eq!((r.groups.len(), r.files_considered), (2, 4));
            // Ordinary files cost their length (Explorer's "Size").
            assert_eq!(r.groups[0].files[0].allocated, r.groups[0].size);

            // A sparse file occupies less than its length ("Size on disk"):
            // only what it really holds is claimed as reclaimable.
            for name in ["s1", "s2"] {
                let f = File::create(t.0.join(name)).unwrap();
                make_sparse(&f);
                f.set_len(5_000_000).unwrap();
            }
            let r = run(&opts(&t.0, 4_000_000));
            let g = &r.groups[0];
            let alloc = g.files[0].allocated;
            assert!(alloc < 5_000_000, "{alloc}");
            assert_eq!(g.size, 5_000_000);
            assert_eq!(g.wasted(), alloc);
        }

        #[test]
        fn hard_link_identity_comes_from_the_file_system() {
            let t = TempDir::new("winlink");
            let data = pattern(3_000, 21);
            let a = t.write("a.bin", &data);
            std::fs::hard_link(&a, t.0.join("b.bin")).unwrap();
            t.write("c.bin", &data);
            let r = run(&opts(&t.0, 1));
            let g = &r.groups[0];
            assert_eq!((g.files.len(), g.families(), g.wasted()), (3, 2, 3_000));
            let fam = |n: &str| g.files.iter().find(|f| f.path.ends_with(n)).unwrap().family;
            assert_eq!(fam("a.bin"), fam("b.bin"));
            assert_ne!(fam("a.bin"), fam("c.bin"));
        }
    }
}
