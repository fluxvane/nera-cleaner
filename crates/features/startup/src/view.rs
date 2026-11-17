//! Every number and label the Login Items screen shows, as pure functions of
//! the scan. The screen renders these strings verbatim and `examples/dump.rs`
//! prints them, so what is checked is what is shown.

use crate::engine::{self, LaunchItem, LoginItem, Scope, Snapshot, Status};
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

/// What the login-items query returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginState {
    NotLoaded,
    Loading,
    Ready(usize),
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tile {
    pub label: &'static str,
    pub value: String,
    pub caption: String,
}

/// Items of one kind: `user = true` for `~/Library/LaunchAgents`, `false` for
/// everything installed system-wide. Apple's are counted only when `show_apple`
/// is on, so the tiles always match the rows the list shows.
fn counted(snapshot: &Snapshot, user: bool, show_apple: bool) -> Vec<&LaunchItem> {
    snapshot
        .items
        .iter()
        .filter(|i| (show_apple || !i.is_apple) && i.scope.is_user() == user)
        .collect()
}

/// `"3 running · 1 turned off"`. Never claims a running count launchd did not give.
fn caption(items: &[&LaunchItem]) -> String {
    let off = items.iter().filter(|i| !i.enabled()).count();
    if items.iter().any(|i| i.loaded.is_none()) {
        return format!("running state unknown · {off} turned off");
    }
    let running = items.iter().filter(|i| i.is_running()).count();
    format!("{running} running · {off} turned off")
}

pub fn tiles(snapshot: &Snapshot, login: LoginState, show_apple: bool) -> [Tile; 3] {
    let user = counted(snapshot, true, show_apple);
    let system = counted(snapshot, false, show_apple);
    let (login_value, login_caption) = match login {
        LoginState::Ready(n) => (n.to_string(), "Open at login".to_string()),
        LoginState::NotLoaded => ("–".to_string(), "Not loaded yet".to_string()),
        LoginState::Loading => ("…".to_string(), "Reading…".to_string()),
        LoginState::Failed => ("–".to_string(), "Not available".to_string()),
    };
    [
        Tile {
            label: "Login items",
            value: login_value,
            caption: login_caption,
        },
        Tile {
            label: "Your agents",
            value: user.len().to_string(),
            caption: caption(&user),
        },
        Tile {
            label: "System-wide",
            value: system.len().to_string(),
            caption: caption(&system),
        },
    ]
}

pub fn page_subtitle(snapshot: Option<&Snapshot>) -> String {
    match snapshot {
        Some(s) => {
            let total = s.items.iter().filter(|i| !i.is_apple).count();
            format!(
                "Apps and agents that start automatically \u{b7} {}",
                format::count(total, "third-party item")
            )
        }
        None => "Apps and agents that start automatically".to_string(),
    }
}

/// Badges on an agent or daemon row, in display order.
pub fn agent_badges(item: &LaunchItem) -> Vec<BadgeView> {
    let mut out = Vec::new();
    if item.is_apple {
        out.push(badge("Apple", BadgeTone::Neutral));
    }
    if item.scope != Scope::User {
        out.push(badge(item.scope.kind_label(), BadgeTone::Neutral));
    }
    if item.program_missing {
        out.push(badge("Program missing", BadgeTone::Warning));
    }
    if item.read_error.is_some() {
        out.push(badge("Can't read", BadgeTone::Warning));
    }
    if item.has_no_label() {
        out.push(badge("Ignored by launchd", BadgeTone::Neutral));
    }
    match item.status() {
        Status::Running => out.push(badge("Running", BadgeTone::Success)),
        Status::Loaded => out.push(badge("Loaded", BadgeTone::Accent)),
        Status::Disabled => out.push(badge("Off", BadgeTone::Neutral)),
        Status::NotLoaded => out.push(badge("Not loaded", BadgeTone::Neutral)),
        Status::Unknown => {}
    }
    // Turned off for the next login but still loaded right now.
    if !item.enabled() && item.is_loaded() {
        out.push(badge("Off at next login", BadgeTone::Neutral));
    }
    out
}

/// Subtitle of a login-item row.
pub fn login_subtitle(item: &LoginItem) -> String {
    if item.path.is_empty() {
        "Location unknown".to_string()
    } else {
        engine::tilde(&item.path)
    }
}

pub fn login_badge(item: &LoginItem) -> Option<&'static str> {
    item.hidden.then_some("Opens hidden")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Loaded, PlistInfo};
    use std::path::PathBuf;

    fn item(label: &str, scope: Scope, apple: bool, loaded: Option<Option<Loaded>>) -> LaunchItem {
        LaunchItem {
            id: format!("/tmp/{label}.plist"),
            plist: PathBuf::from(format!("/tmp/{label}.plist")),
            scope,
            label: label.into(),
            info: Some(PlistInfo {
                label: Some(label.into()),
                ..Default::default()
            }),
            read_error: None,
            vendor: "Test".into(),
            is_apple: apple,
            legacy_hidden: false,
            override_disabled: None,
            loaded,
            program_missing: false,
        }
    }

    fn run(pid: i32) -> Option<Option<Loaded>> {
        Some(Some(Loaded {
            pid: Some(pid),
            last_exit: None,
        }))
    }

    #[test]
    fn tiles_count_third_party_items_only() {
        let mut off = item("c", Scope::User, false, Some(None));
        off.override_disabled = Some(true);
        let mut loaded_but_off = item("d", Scope::User, false, run(9));
        loaded_but_off.override_disabled = Some(true);
        let snap = Snapshot {
            items: vec![
                item("a", Scope::User, false, run(1)),
                item("b", Scope::User, true, run(2)), // Apple: not counted
                off,
                loaded_but_off,
                item("e", Scope::Daemon, false, run(3)),
                item(
                    "f",
                    Scope::GlobalAgent,
                    false,
                    Some(Some(Loaded {
                        pid: None,
                        last_exit: Some(0),
                    })),
                ),
            ],
            ..Default::default()
        };
        let t = tiles(&snap, LoginState::Ready(4), false);
        assert_eq!(
            (t[0].value.as_str(), t[0].caption.as_str()),
            ("4", "Open at login")
        );
        // a and d run, c and d are turned off.
        assert_eq!(
            (t[1].value.as_str(), t[1].caption.as_str()),
            ("3", "2 running · 2 turned off")
        );
        assert_eq!(
            (t[2].value.as_str(), t[2].caption.as_str()),
            ("2", "1 running · 0 turned off")
        );
        // With "Apple items" on, the tiles count the Apple row too.
        let all = tiles(&snap, LoginState::Ready(4), true);
        assert_eq!(
            (all[1].value.as_str(), all[1].caption.as_str()),
            ("4", "3 running · 2 turned off")
        );
        assert_eq!(
            page_subtitle(Some(&snap)),
            "Apps and agents that start automatically \u{b7} 5 third-party items"
        );
        assert_eq!(
            page_subtitle(None),
            "Apps and agents that start automatically"
        );
        assert_eq!(
            tiles(&snap, LoginState::Loading, false)[0].caption,
            "Reading…"
        );
        assert_eq!(tiles(&snap, LoginState::Failed, false)[0].value, "–");
        assert_eq!(
            tiles(&snap, LoginState::NotLoaded, false)[0].caption,
            "Not loaded yet"
        );
    }

    #[test]
    fn running_count_is_not_invented_when_launchd_could_not_be_read() {
        let snap = Snapshot {
            items: vec![item("a", Scope::User, false, None)],
            ..Default::default()
        };
        assert_eq!(
            tiles(&snap, LoginState::Loading, false)[1].caption,
            "running state unknown · 0 turned off"
        );
    }

    #[test]
    fn badges_follow_the_real_state() {
        let labels = |i: &LaunchItem| {
            agent_badges(i)
                .into_iter()
                .map(|b| b.label)
                .collect::<Vec<_>>()
        };
        assert_eq!(labels(&item("a", Scope::User, false, run(1))), ["Running"]);
        assert_eq!(
            labels(&item("a", Scope::Daemon, false, Some(None))),
            ["Daemon", "Not loaded"]
        );
        let mut off = item("a", Scope::GlobalAgent, true, Some(None));
        off.override_disabled = Some(true);
        assert_eq!(labels(&off), ["Apple", "Agent", "Off"]);
        let mut still_loaded = item(
            "a",
            Scope::User,
            false,
            Some(Some(Loaded {
                pid: None,
                last_exit: Some(0),
            })),
        );
        still_loaded.override_disabled = Some(true);
        assert_eq!(labels(&still_loaded), ["Loaded", "Off at next login"]);
        assert_eq!(
            labels(&item("a", Scope::User, false, None)),
            Vec::<&str>::new()
        );
        let mut broken = item("a", Scope::Daemon, false, Some(None));
        broken.read_error = Some("x".into());
        broken.program_missing = true;
        assert_eq!(
            labels(&broken),
            ["Daemon", "Program missing", "Can't read", "Not loaded"]
        );
        let mut empty = item("a", Scope::User, false, Some(None));
        empty.info = Some(PlistInfo::default());
        assert_eq!(labels(&empty), ["Ignored by launchd", "Not loaded"]);
    }

    #[test]
    fn login_rows() {
        let named = LoginItem {
            name: "Slack".into(),
            path: "/Applications/Slack.app".into(),
            hidden: true,
        };
        assert_eq!(login_subtitle(&named), "/Applications/Slack.app");
        assert_eq!(login_badge(&named), Some("Opens hidden"));
        let ghost = LoginItem {
            name: "Ghost".into(),
            path: String::new(),
            hidden: false,
        };
        assert_eq!(login_subtitle(&ghost), "Location unknown");
        assert_eq!(login_badge(&ghost), None);
    }
}
