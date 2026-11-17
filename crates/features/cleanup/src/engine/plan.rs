//! Pure selection and presentation logic for the System Junk screen:
//! auto-selection, grouping and the "Trash or permanent" wording.

use super::scan::Scanned;
use super::targets::{Category, Group};
use super::AUTO_SELECT_MIN;
use std::collections::HashSet;

/// Ids pre-selected after a scan: safe categories of at least 1 MB.
pub fn auto_selection(found: &[Scanned]) -> HashSet<&'static str> {
    found
        .iter()
        .filter(|s| s.category.safe && s.size >= AUTO_SELECT_MIN)
        .map(|s| s.category.id)
        .collect()
}

/// Ids of every safe category with something to remove ("Select safe").
pub fn safe_selection(found: &[Scanned]) -> HashSet<&'static str> {
    found
        .iter()
        .filter(|s| s.category.safe && s.size > 0)
        .map(|s| s.category.id)
        .collect()
}

/// Non-empty categories per group, in screen order, biggest first.
pub fn grouped(found: &[Scanned]) -> Vec<(Group, Vec<&Scanned>)> {
    Group::ALL
        .iter()
        .filter_map(|&group| {
            let mut items: Vec<&Scanned> = found
                .iter()
                .filter(|s| s.category.group == group && s.size > 0)
                .collect();
            items.sort_by(|a, b| {
                b.size
                    .cmp(&a.size)
                    .then(a.category.name.cmp(b.category.name))
            });
            (!items.is_empty()).then_some((group, items))
        })
        .collect()
}

/// Names of categories with nothing in them.
pub fn empty_names(found: &[Scanned]) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = found
        .iter()
        .filter(|s| s.size == 0 && s.blocked == 0)
        .map(|s| s.category.name)
        .collect();
    names.sort_unstable();
    names
}

/// Categories that hold data this user may not remove (owned by the system),
/// with its allocated size, by name.
pub fn blocked_categories(found: &[Scanned]) -> Vec<(&'static str, u64)> {
    let mut out: Vec<(&'static str, u64)> = found
        .iter()
        .filter(|s| s.size == 0 && s.blocked > 0)
        .map(|s| (s.category.name, s.blocked))
        .collect();
    out.sort_unstable();
    out
}

/// How a selection will be removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Disposal {
    /// Categories whose items go to the Trash.
    pub trash: Vec<&'static str>,
    /// Categories deleted permanently (Trash emptying, or Trash disabled).
    pub permanent: Vec<&'static str>,
}

impl Disposal {
    pub fn of(selected: &[&Category], move_to_trash: bool) -> Self {
        let mut d = Disposal::default();
        for c in selected {
            if c.always_trashes() || (move_to_trash && !c.empties_trash()) {
                d.trash.push(c.name);
            } else {
                d.permanent.push(c.name);
            }
        }
        d
    }

    /// Short footer label.
    pub fn short(&self) -> &'static str {
        match (self.trash.is_empty(), self.permanent.is_empty()) {
            (_, true) => concat!("Moves to the ", nera_core::trash_name!()),
            (true, false) => "Deletes permanently",
            (false, false) => "Some items are deleted permanently",
        }
    }

    /// Sentence for the confirmation dialog.
    pub fn sentence(&self) -> String {
        match (self.trash.is_empty(), self.permanent.is_empty()) {
            (_, true) => concat!(
                "Items are moved to the ",
                nera_core::trash_name!(),
                ", so you can restore them. The space is freed when you empty the ",
                nera_core::trash_name!(),
                "."
            )
            .into(),
            (true, false) => "Items are deleted permanently and cannot be restored.".into(),
            (false, false) => {
                let names: Vec<&str> = self.permanent.iter().copied().take(3).collect();
                let more = self.permanent.len().saturating_sub(names.len());
                let mut list = names.join(", ");
                if more > 0 {
                    list.push_str(&format!(" and {more} more"));
                }
                let trash = nera_core::os::TRASH;
                format!(
                    "These are deleted permanently and cannot be restored: {list}. Everything else is moved to the {trash}."
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::targets::Mode;
    use std::path::PathBuf;

    fn scanned(
        id: &'static str,
        group: Group,
        size: u64,
        safe: bool,
        mode: Mode,
        path: &str,
    ) -> Scanned {
        Scanned {
            category: Category {
                id,
                name: id,
                description: "",
                icon: "folder",
                group,
                paths: vec![PathBuf::from(path)],
                safe,
                mode,
            },
            size,
            blocked: 0,
        }
    }

    fn sample() -> Vec<Scanned> {
        vec![
            scanned(
                "big",
                Group::Caches,
                5_000_000,
                true,
                Mode::Contents,
                "/u/a",
            ),
            scanned("small", Group::Caches, 10, true, Mode::Contents, "/u/b"),
            scanned(
                "review",
                Group::Developer,
                9_000_000,
                false,
                Mode::Contents,
                "/u/c",
            ),
            scanned("empty", Group::Logs, 0, true, Mode::Contents, "/u/d"),
            scanned(
                "vm",
                Group::AiVm,
                2_000_000,
                false,
                Mode::TrashTargets,
                "/u/e",
            ),
            scanned(
                "trash",
                Group::System,
                3_000_000,
                true,
                Mode::Contents,
                crate::engine::targets::TEST_TRASH,
            ),
        ]
    }

    #[test]
    fn auto_selection_skips_unsafe_and_tiny() {
        let sel = auto_selection(&sample());
        assert!(sel.contains("big") && sel.contains("trash"));
        assert!(
            !sel.contains("small")
                && !sel.contains("review")
                && !sel.contains("vm")
                && !sel.contains("empty")
        );
        let safe = safe_selection(&sample());
        assert!(safe.contains("small") && !safe.contains("review") && !safe.contains("empty"));
    }

    #[test]
    fn groups_hide_empty_categories_and_sort_by_size() {
        let found = sample();
        let groups = grouped(&found);
        let order: Vec<Group> = groups.iter().map(|(g, _)| *g).collect();
        assert_eq!(
            order,
            vec![Group::Caches, Group::Developer, Group::System, Group::AiVm]
        );
        assert_eq!(groups[0].1[0].category.id, "big");
        assert_eq!(empty_names(&found), vec!["empty"]);
    }

    #[test]
    fn disposal_wording() {
        let found = sample();
        let pick = |ids: &[&str]| -> Vec<&Category> {
            found
                .iter()
                .map(|s| &s.category)
                .filter(|c| ids.contains(&c.id))
                .collect()
        };
        let d = Disposal::of(&pick(&["big"]), true);
        assert_eq!(
            d.short(),
            concat!("Moves to the ", nera_core::trash_name!())
        );
        assert!(d.sentence().contains("restore"));
        let d = Disposal::of(&pick(&["big"]), false);
        assert_eq!(d.short(), "Deletes permanently");
        assert!(d.sentence().contains("permanently"));
        // VM folders always go to the Trash, the Trash itself is always permanent.
        let d = Disposal::of(&pick(&["vm"]), false);
        assert_eq!(
            d.short(),
            concat!("Moves to the ", nera_core::trash_name!())
        );
        let d = Disposal::of(&pick(&["big", "trash"]), true);
        assert_eq!(d.permanent, vec!["trash"]);
        assert!(d
            .sentence()
            .contains("deleted permanently and cannot be restored: trash"));
        assert_eq!(d.short(), "Some items are deleted permanently");
    }
}
