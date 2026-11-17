//! Every number and label the startup screen shows on Windows, as pure
//! functions of the scan. The screen renders these strings verbatim and
//! `examples/dump_startup.rs` prints them, so what is checked is what is shown.

use crate::engine::{Scope, Snapshot, StartupItem, State};
use nera_core::format;

/// How a badge is coloured (the screen maps these to theme tones).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadgeTone {
    Neutral,
    Accent,
    Success,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadgeView {
    pub label: &'static str,
    pub tone: BadgeTone,
}

fn badge(label: &'static str, tone: BadgeTone) -> BadgeView {
    BadgeView { label, tone }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tile {
    pub label: &'static str,
    pub value: String,
    pub caption: String,
}

/// `"3 running · 1 turned off"`. Never claims a running count it could not see.
fn caption(items: &[&StartupItem]) -> String {
    let off = items.iter().filter(|i| !i.enabled()).count();
    if items.iter().any(|i| i.running.is_none()) {
        return format!("{off} turned off");
    }
    let running = items.iter().filter(|i| i.running == Some(true)).count();
    format!("{running} running \u{b7} {off} turned off")
}

pub fn tiles(snapshot: &Snapshot) -> [Tile; 3] {
    let tile = |label, scope| {
        let items: Vec<&StartupItem> = snapshot.in_scope(scope).collect();
        Tile {
            label,
            value: items.len().to_string(),
            caption: caption(&items),
        }
    };
    [
        tile("Your apps", Scope::User),
        tile("All users", Scope::Machine),
        tile("Store apps", Scope::Store),
    ]
}

pub fn page_subtitle(snapshot: Option<&Snapshot>) -> String {
    const BASE: &str = "Apps that start automatically when you sign in";
    match snapshot {
        Some(s) => format!(
            "{BASE} \u{b7} {}",
            format::count(s.items.len(), "startup app")
        ),
        None => BASE.to_string(),
    }
}

/// Badges on a row, in display order.
pub fn badges(item: &StartupItem) -> Vec<BadgeView> {
    let mut out = Vec::new();
    if item.target_missing {
        out.push(badge("Program missing", BadgeTone::Warning));
    }
    match item.state {
        State::On => {}
        State::Off => out.push(badge("Off", BadgeTone::Neutral)),
        State::OffByPolicy => out.push(badge("Off by policy", BadgeTone::Neutral)),
        State::OnByPolicy => out.push(badge("On by policy", BadgeTone::Accent)),
    }
    if item.running == Some(true) {
        out.push(badge("Running", BadgeTone::Success));
    }
    out
}

/// One line under the name: publisher · where it comes from · program.
pub fn subtitle(item: &StartupItem) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(p) = &item.publisher {
        parts.push(p.clone());
    }
    parts.push(item.source.label().to_string());
    let location = item.location();
    if !location.is_empty() {
        parts.push(location);
    }
    parts.join(" \u{b7} ")
}

/// Section descriptions. "All users" says read-only only when nothing there
/// can be changed by this process.
pub fn section_description(scope: Scope, snapshot: &Snapshot) -> &'static str {
    match scope {
        Scope::User => "Start when you sign in, from your Run registry key and Startup folder. Turning one off works like Task Manager: nothing is deleted.",
        Scope::Machine if snapshot.in_scope(Scope::Machine).any(|i| i.is_editable()) => {
            "Start for everyone who signs in to this PC."
        }
        Scope::Machine => "Start for everyone who signs in to this PC. Changing them needs an administrator, so they are read-only here.",
        Scope::Store => "Startup tasks of apps from the Microsoft Store. Turn them on or off in Settings > Apps > Startup.",
    }
}

/// (title, message) when a section has no rows; `has_any` = rows exist but
/// none match the search.
pub fn empty_state(scope: Scope, has_any: bool) -> (&'static str, &'static str) {
    if has_any {
        return ("Nothing to show", "No startup apps match your search.");
    }
    match scope {
        Scope::User => (
            "No startup apps",
            "Nothing in your Run key or Startup folder starts automatically.",
        ),
        Scope::Machine => (
            "No startup apps for all users",
            "Nothing in the machine Run keys or the all-users Startup folder.",
        ),
        Scope::Store => (
            "No Store startup tasks",
            "No installed Store app has a startup task.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Hive, Source};
    use std::path::PathBuf;

    fn item(name: &str, source: Source, state: State, running: Option<bool>) -> StartupItem {
        StartupItem {
            id: format!("test:{name}"),
            entry: name.into(),
            name: name.into(),
            source,
            command: format!(r"C:\Apps\{name}.exe --x"),
            target: Some(PathBuf::from(format!(r"C:\Apps\{name}.exe"))),
            publisher: None,
            state,
            approved: None,
            target_missing: false,
            running,
            writable: matches!(
                source,
                Source::Run {
                    hive: Hive::User,
                    ..
                } | Source::Folder(Hive::User)
            ),
        }
    }

    const USER_RUN: Source = Source::Run {
        hive: Hive::User,
        wow64: false,
    };
    const MACHINE_RUN: Source = Source::Run {
        hive: Hive::Machine,
        wow64: false,
    };

    fn snapshot() -> Snapshot {
        Snapshot {
            items: vec![
                item("OneDrive", USER_RUN, State::On, Some(true)),
                item("Docker", USER_RUN, State::Off, Some(false)),
                item("Tool", Source::Folder(Hive::User), State::On, Some(false)),
                item("SecurityHealth", MACHINE_RUN, State::On, Some(true)),
                item("Slack", Source::Store, State::On, None),
                item("Terminal", Source::Store, State::Off, Some(false)),
            ],
            errors: Vec::new(),
        }
    }

    #[test]
    fn tiles_count_each_section() {
        let t = tiles(&snapshot());
        assert_eq!(
            (t[0].label, t[0].value.as_str(), t[0].caption.as_str()),
            ("Your apps", "3", "1 running · 1 turned off")
        );
        assert_eq!(
            (t[1].label, t[1].value.as_str(), t[1].caption.as_str()),
            ("All users", "1", "1 running · 0 turned off")
        );
        // One Store task's program is unknown: no running count is claimed.
        assert_eq!(
            (t[2].label, t[2].value.as_str(), t[2].caption.as_str()),
            ("Store apps", "2", "1 turned off")
        );
        assert_eq!(
            page_subtitle(Some(&snapshot())),
            "Apps that start automatically when you sign in · 6 startup apps"
        );
        assert_eq!(
            page_subtitle(None),
            "Apps that start automatically when you sign in"
        );
    }

    #[test]
    fn badges_follow_the_real_state() {
        let labels = |i: &StartupItem| badges(i).into_iter().map(|b| b.label).collect::<Vec<_>>();
        assert_eq!(
            labels(&item("a", USER_RUN, State::On, Some(true))),
            ["Running"]
        );
        assert_eq!(
            labels(&item("a", USER_RUN, State::Off, Some(false))),
            ["Off"]
        );
        assert!(labels(&item("a", USER_RUN, State::On, None)).is_empty());
        let mut missing = item("a", USER_RUN, State::Off, Some(false));
        missing.target_missing = true;
        assert_eq!(labels(&missing), ["Program missing", "Off"]);
        assert_eq!(
            labels(&item("a", Source::Store, State::OffByPolicy, None)),
            ["Off by policy"]
        );
        assert_eq!(
            badges(&item("a", Source::Store, State::OnByPolicy, None))[0].tone,
            BadgeTone::Accent
        );
    }

    #[test]
    fn subtitles_and_sections() {
        let mut a = item("OneDrive", USER_RUN, State::On, Some(true));
        a.publisher = Some("Microsoft Corporation".into());
        assert_eq!(
            subtitle(&a),
            r"Microsoft Corporation · Registry · C:\Apps\OneDrive.exe"
        );
        a.target = None;
        assert_eq!(
            subtitle(&a),
            r"Microsoft Corporation · Registry · C:\Apps\OneDrive.exe --x"
        );
        let mut s = item("Slack", Source::Store, State::On, None);
        s.target = None;
        assert_eq!(subtitle(&s), "Microsoft Store app");

        let snap = snapshot();
        assert!(section_description(Scope::Machine, &snap).ends_with("read-only here."));
        let mut elevated = snap.clone();
        for i in &mut elevated.items {
            i.writable = true;
        }
        assert_eq!(
            section_description(Scope::Machine, &elevated),
            "Start for everyone who signs in to this PC."
        );
        assert_eq!(empty_state(Scope::Store, true).0, "Nothing to show");
        assert_eq!(empty_state(Scope::User, false).0, "No startup apps");
    }
}
