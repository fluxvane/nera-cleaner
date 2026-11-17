//! What a cleanup removes from one target, item by item. The scan sizes
//! exactly these items and `clean` removes exactly these items, so the size
//! shown before cleaning is the size cleaning acts on.

use super::scan::{old_installers, Options};
use super::size::Removal;
use super::targets::{is_trash_dir, Category, Mode};
use nera_core::paths::{is_under, is_under_any};
use nera_core::safety;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// How `category` removes its items: Trash-emptying and the Trash-disabled
/// preference delete permanently, VM disks and downloads always go to the Trash.
pub fn removal_for(category: &Category, move_to_trash: bool) -> Removal {
    if category.always_trashes() || (move_to_trash && !category.empties_trash()) {
        Removal::Trash
    } else {
        Removal::Permanent
    }
}

/// Items of `target` that cleaning removes, skipping `skip` (subtrees owned by
/// other categories or already cleaned in this run) and the user's exclusions.
pub fn removal_items(
    category: &Category,
    target: &Path,
    skip: &[PathBuf],
    opts: &Options,
) -> Vec<(PathBuf, Removal)> {
    // The Recycle Bin is measured and emptied by the shell, as one item.
    #[cfg(windows)]
    if is_trash_dir(target) {
        return if is_under_any(target, &opts.excluded) {
            Vec::new()
        } else {
            vec![(target.to_path_buf(), Removal::Permanent)]
        };
    }
    let Ok(meta) = std::fs::symlink_metadata(target) else {
        return Vec::new();
    };
    if is_under_any(target, &opts.excluded) || safety::running_ide_owning(target).is_some() {
        return Vec::new();
    }
    let removal = removal_for(category, opts.to_trash);
    if !meta.is_dir() {
        // Single-file target (e.g. state.vscdb.backup).
        let removal = if category.mode == Mode::Contents {
            removal
        } else {
            Removal::Trash
        };
        return vec![(target.to_path_buf(), removal)];
    }
    let allowed = |p: &Path| !is_under_any(p, skip) && !is_under_any(p, &opts.excluded);
    match category.mode {
        Mode::OldInstallers => old_installers(target, SystemTime::now())
            .into_iter()
            .map(|(path, _)| path)
            .filter(|p| allowed(p))
            .map(|p| (p, Removal::Trash))
            .collect(),
        // Trash the whole target unless something inside must be kept.
        Mode::TrashTargets if skip.iter().all(|s| !is_under(s, target)) => {
            if safety::is_trashable(target) {
                vec![(target.to_path_buf(), Removal::Trash)]
            } else {
                Vec::new()
            }
        }
        // Children of the target (the folder itself stays).
        Mode::TrashTargets | Mode::Contents => {
            let emptying_trash = is_trash_dir(target);
            let mut out: Vec<(PathBuf, Removal)> = std::fs::read_dir(target)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| emptying_trash || opts.include_hidden || !is_hidden(e))
                .map(|e| e.path())
                .filter(|p| allowed(p) && safety::is_deletable(p))
                .map(|p| (p, removal))
                .collect();
            out.sort_by(|a, b| a.0.cmp(&b.0));
            out
        }
    }
}

/// A dotfile, or on Windows also an entry with the hidden attribute.
fn is_hidden(entry: &std::fs::DirEntry) -> bool {
    entry.file_name().to_string_lossy().starts_with('.') || hidden_attribute(entry)
}

#[cfg(target_os = "macos")]
fn hidden_attribute(_entry: &std::fs::DirEntry) -> bool {
    false
}

#[cfg(windows)]
fn hidden_attribute(entry: &std::fs::DirEntry) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    entry
        .metadata()
        .is_ok_and(|m| m.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::targets::Group;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nera-items-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cat(mode: Mode, paths: Vec<PathBuf>) -> Category {
        Category {
            id: "c",
            name: "c",
            description: "",
            icon: "folder",
            group: Group::Caches,
            paths,
            safe: true,
            mode,
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn hidden_children_are_skipped_but_trash_includes_them() {
        let root = tmp("hidden");
        std::fs::create_dir_all(root.join("Caches/app")).unwrap();
        std::fs::create_dir_all(root.join("Caches/.dot")).unwrap();
        std::fs::create_dir_all(root.join(".Trash/.hidden")).unwrap();
        let opts = Options::default();
        let items = removal_items(
            &cat(Mode::Contents, vec![]),
            &root.join("Caches"),
            &[],
            &opts,
        );
        assert_eq!(items, vec![(root.join("Caches/app"), Removal::Permanent)]);
        let trash = removal_items(
            &cat(Mode::Contents, vec![root.join(".Trash")]),
            &root.join(".Trash"),
            &[],
            &opts,
        );
        assert_eq!(
            trash,
            vec![(root.join(".Trash/.hidden"), Removal::Permanent)]
        );
        let with_hidden = Options {
            include_hidden: true,
            to_trash: true,
            ..Default::default()
        };
        let items = removal_items(
            &cat(Mode::Contents, vec![]),
            &root.join("Caches"),
            &[],
            &with_hidden,
        );
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|(_, r)| *r == Removal::Trash));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn hidden_attribute_is_skipped_and_the_recycle_bin_is_one_item() {
        let root = tmp("hidden-win");
        std::fs::create_dir_all(root.join(r"Caches\app")).unwrap();
        std::fs::create_dir_all(root.join(r"Caches\.dot")).unwrap();
        std::fs::write(root.join(r"Caches\thumbs.db"), b"x").unwrap();
        let hidden = std::process::Command::new("attrib")
            .arg("+h")
            .arg(root.join(r"Caches\thumbs.db"))
            .status()
            .unwrap();
        assert!(hidden.success());
        let c = cat(Mode::Contents, vec![]);
        let items = removal_items(&c, &root.join("Caches"), &[], &Options::default());
        assert_eq!(items, vec![(root.join(r"Caches\app"), Removal::Permanent)]);
        let with_hidden = Options {
            include_hidden: true,
            ..Default::default()
        };
        assert_eq!(
            removal_items(&c, &root.join("Caches"), &[], &with_hidden).len(),
            3
        );
        std::fs::remove_dir_all(&root).unwrap();

        // Never cleaned here: only the list of items is checked.
        let bin = nera_core::paths::system_drive().join("$Recycle.Bin");
        let trash = cat(Mode::Contents, vec![bin.clone()]);
        assert!(trash.empties_trash());
        assert_eq!(
            removal_items(&trash, &bin, &[], &with_hidden),
            vec![(bin.clone(), Removal::Permanent)]
        );
        let excluded = Options {
            excluded: vec![bin.clone()],
            ..Default::default()
        };
        assert!(removal_items(&trash, &bin, &[], &excluded).is_empty());
    }

    #[test]
    fn trash_targets_move_whole_unless_something_inside_is_kept() {
        let root = tmp("whole");
        std::fs::create_dir_all(root.join("vm/keep")).unwrap();
        std::fs::create_dir_all(root.join("vm/other")).unwrap();
        let c = cat(Mode::TrashTargets, vec![root.join("vm")]);
        let opts = Options::default();
        assert_eq!(
            removal_items(&c, &root.join("vm"), &[], &opts),
            vec![(root.join("vm"), Removal::Trash)]
        );
        let items = removal_items(&c, &root.join("vm"), &[root.join("vm/keep")], &opts);
        assert_eq!(items, vec![(root.join("vm/other"), Removal::Trash)]);
        std::fs::remove_dir_all(root).unwrap();
    }
}
