//! Every junk category Nera knows about (port of `ScanTargets.swift`).
//!
//! Building the list reads the file system (other user accounts, per-user
//! temp dirs, app support folders, idle IDE system dirs), so call
//! [`all_categories`] from background work only. The types are shared; the
//! locations live in `targets_macos.rs` and `targets_windows.rs`.

use std::path::{Path, PathBuf};

/// Sections of the System Junk screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Group {
    Caches,
    Logs,
    Browsers,
    Developer,
    Apps,
    System,
    AiVm,
}

impl Group {
    pub const ALL: [Group; 7] = [
        Group::Caches,
        Group::Logs,
        Group::Browsers,
        Group::Developer,
        Group::Apps,
        Group::System,
        Group::AiVm,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Group::Caches => "Caches",
            Group::Logs => "Logs",
            Group::Browsers => "Browsers",
            Group::Developer => "Developer tools",
            Group::Apps => "Apps",
            Group::System => "System",
            Group::AiVm => "AI and virtual machines",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Group::Caches => "folder",
            Group::Logs => "file-text",
            Group::Browsers => "globe",
            Group::Developer => "code-xml",
            Group::Apps => "app-window",
            Group::System => "monitor-cog",
            Group::AiVm => "brain",
        }
    }
}

/// How a category's targets are removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Remove the CONTENTS of each target directory (the folder stays).
    /// A `.Trash` target is emptied permanently.
    Contents,
    /// Move each target itself to the Trash (VM disks, models, agent data).
    TrashTargets,
    /// Move `.dmg`/`.pkg` files older than 30 days in the target to the Trash.
    OldInstallers,
}

#[derive(Clone, Debug)]
pub struct Category {
    /// Stable domain id (row id, selection key).
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// Lucide icon name.
    pub icon: &'static str,
    pub group: Group,
    pub paths: Vec<PathBuf>,
    /// False: needs review; never auto-selected.
    pub safe: bool,
    pub mode: Mode,
}

impl Category {
    fn new(
        id: &'static str,
        name: &'static str,
        icon: &'static str,
        group: Group,
        paths: Vec<PathBuf>,
        description: &'static str,
    ) -> Self {
        Self {
            id,
            name,
            description,
            icon,
            group,
            paths,
            safe: true,
            mode: Mode::Contents,
        }
    }

    fn review(mut self) -> Self {
        self.safe = false;
        self
    }

    #[cfg_attr(windows, allow(dead_code))]
    fn trash_targets(mut self) -> Self {
        self.mode = Mode::TrashTargets;
        self
    }

    /// True when cleaning this category always uses the Trash, whatever the
    /// "move to Trash" preference says.
    pub fn always_trashes(&self) -> bool {
        matches!(self.mode, Mode::TrashTargets | Mode::OldInstallers)
    }

    /// True when cleaning empties a Trash folder (permanent).
    pub fn empties_trash(&self) -> bool {
        self.mode == Mode::Contents
            && !self.paths.is_empty()
            && self.paths.iter().all(|p| is_trash_dir(p))
    }
}

/// `…/.Trash`
#[cfg(target_os = "macos")]
pub fn is_trash_dir(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n == ".Trash")
}

/// `<drive>:\$Recycle.Bin`: the Recycle Bin, measured and emptied through the
/// shell (`SHQueryRecycleBinW` / `SHEmptyRecycleBinW`), never file by file.
/// Only the folder at a drive root qualifies, so a look-alike folder elsewhere
/// (a test fixture, a copied disk) is never mistaken for it.
#[cfg(windows)]
pub fn is_trash_dir(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case("$Recycle.Bin"))
        && path.parent().is_some_and(|drive| drive.parent().is_none())
}

/// A Trash folder for tests that only look at the path (never cleaned).
#[cfg(all(test, target_os = "macos"))]
pub(crate) const TEST_TRASH: &str = "/u/.Trash";
#[cfg(all(test, windows))]
pub(crate) const TEST_TRASH: &str = r"C:\$Recycle.Bin";

/// Sorted child directories (not following symlinks).
fn child_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

// The list of locations differs per platform; both export `all_categories`.
#[cfg(target_os = "macos")]
#[path = "targets_macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "targets_windows.rs"]
mod platform;
pub use platform::*;

#[cfg(test)]
mod shared_tests {
    use super::*;

    #[test]
    fn trash_dir_is_recognised_by_path_only() {
        assert!(is_trash_dir(Path::new(TEST_TRASH)));
        assert!(!is_trash_dir(Path::new("/u/Library/Caches")));
        #[cfg(windows)]
        {
            // A look-alike folder that is not at a drive root is not the Recycle Bin.
            assert!(!is_trash_dir(Path::new(r"C:\Users\me\$Recycle.Bin")));
            assert!(!is_trash_dir(Path::new(r"C:\Users\me\.Trash")));
            assert!(is_trash_dir(Path::new(r"D:\$RECYCLE.BIN")));
        }
    }
}
