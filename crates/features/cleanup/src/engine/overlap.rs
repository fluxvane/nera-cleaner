//! Nested paths across categories (port of the fixed `CategoryPathOverlap`).
//!
//! Several categories list nested paths (User caches `~/Library/Caches` vs
//! Chrome `~/Library/Caches/Google/Chrome`). Cleaning a root removes its
//! immediate children, so a broad category must skip the WHOLE immediate child
//! that contains another category's path. Scan totals and cleaning use the
//! same exclusions, so a category never removes more than it reported.

use super::targets::Category;
use nera_core::paths::standardize;
use std::path::{Path, PathBuf};

/// Owner id used for paths no category may sweep up as part of a parent.
pub const RESERVED: &str = "__reserved__";

/// (owner id, standardized path) for every path of every category, plus
/// reserved paths (IDE roots, Nera's own caches) and user exclusions.
#[derive(Clone, Debug, Default)]
pub struct PathIndex {
    entries: Vec<(&'static str, PathBuf)>,
}

impl PathIndex {
    /// Index over ALL categories (selected or not) and `extra_reserved`.
    pub fn build(categories: &[Category], extra_reserved: &[PathBuf]) -> Self {
        let mut entries: Vec<(&'static str, PathBuf)> = categories
            .iter()
            .flat_map(|c| c.paths.iter().map(move |p| (c.id, standardize(p))))
            .collect();
        entries.extend(extra_reserved.iter().map(|p| (RESERVED, standardize(p))));
        Self { entries }
    }

    /// Subtrees of `root` that hold paths owned by OTHER owners: the immediate
    /// child of `root` on the way to each such path.
    pub fn descendant_exclusions(&self, root: &Path, owner: &str) -> Vec<PathBuf> {
        let root = standardize(root);
        let mut out: Vec<PathBuf> = Vec::new();
        for (id, path) in &self.entries {
            if *id == owner || *path == root {
                continue;
            }
            let Ok(rest) = path.strip_prefix(&root) else {
                continue;
            };
            let Some(first) = rest.components().next() else {
                continue;
            };
            let top = root.join(first.as_os_str());
            if !out.contains(&top) {
                out.push(top);
            }
        }
        out
    }
}

/// Paths a broad category must never sweep up as part of a parent folder:
/// the JetBrains / Android Studio roots and Nera's own caches.
pub fn reserved_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for family in nera_core::safety::ide_families() {
        out.push(family.caches_root.clone());
        out.push(family.logs_root.clone());
    }
    #[cfg(target_os = "macos")]
    {
        let caches = nera_core::paths::home_join("Library/Caches");
        out.push(caches.join(nera_core::paths::BUNDLE_ID));
        out.push(caches.join("io.sentry"));
    }
    // Nera's own settings (`%APPDATA%\Nera Cleaner`) and caches
    // (`%LOCALAPPDATA%\Nera Cleaner`).
    #[cfg(windows)]
    {
        out.push(nera_core::paths::app_support_dir());
        if let Some(dir) = nera_core::fs_util::app_cache_dir().parent() {
            out.push(dir.to_path_buf());
        }
    }
    out
}

/// Cleaning order: Trash-emptying categories first (so items other
/// categories move to the Trash in this run are kept), then narrow targets
/// before broad parents (Chrome before User caches).
pub fn cleaning_order(categories: &[Category]) -> Vec<Category> {
    let mut out = categories.to_vec();
    out.sort_by_key(|c| (!c.empties_trash(), std::cmp::Reverse(specificity(c))));
    out
}

fn specificity(c: &Category) -> usize {
    c.paths
        .iter()
        .map(|p| standardize(p).components().count())
        .max()
        .unwrap_or(0)
}

/// True when every target of `category` lies under a root already cleaned in
/// this run.
pub fn targets_already_covered(category: &Category, cleaned_roots: &[PathBuf]) -> bool {
    !cleaned_roots.is_empty()
        && !category.paths.is_empty()
        && category
            .paths
            .iter()
            .all(|p| nera_core::paths::is_under_any(p, cleaned_roots))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::targets::{Group, Mode};

    fn cat(id: &'static str, paths: &[&str]) -> Category {
        Category {
            id,
            name: id,
            description: "",
            icon: "folder",
            group: Group::Caches,
            paths: paths.iter().map(PathBuf::from).collect(),
            safe: true,
            mode: Mode::Contents,
        }
    }

    #[test]
    fn excludes_whole_immediate_child() {
        let cats = vec![
            cat("user", &["/u/Library/Caches"]),
            cat("chrome", &["/u/Library/Caches/Google/Chrome"]),
            cat("jb", &["/u/Library/Caches/JetBrains/Rider/caches"]),
        ];
        let idx = PathIndex::build(&cats, &[PathBuf::from("/u/Library/Caches/me.own")]);
        let ex = idx.descendant_exclusions(Path::new("/u/Library/Caches"), "user");
        assert_eq!(
            ex,
            vec![
                PathBuf::from("/u/Library/Caches/Google"),
                PathBuf::from("/u/Library/Caches/JetBrains"),
                PathBuf::from("/u/Library/Caches/me.own"),
            ]
        );
        assert!(idx
            .descendant_exclusions(Path::new("/u/Library/Caches/Google/Chrome"), "chrome")
            .is_empty());
    }

    #[test]
    fn orders_trash_then_deepest_first() {
        let mut trash = cat("trash", &[crate::engine::targets::TEST_TRASH]);
        trash.mode = Mode::Contents;
        let cats = vec![
            cat("user", &["/u/Library/Caches"]),
            trash,
            cat("chrome", &["/u/Library/Caches/Google/Chrome"]),
        ];
        let ids: Vec<_> = cleaning_order(&cats).iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["trash", "chrome", "user"]);
    }

    #[test]
    fn detects_covered_targets() {
        let c = cat("pip", &["/u/.cache/pip"]);
        assert!(targets_already_covered(&c, &[PathBuf::from("/u/.cache")]));
        assert!(!targets_already_covered(
            &c,
            &[PathBuf::from("/u/.cache/other")]
        ));
        assert!(!targets_already_covered(&c, &[]));
    }
}
