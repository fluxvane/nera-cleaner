//! Read-only dump of everything the startup screen would show on this computer.
//! `cargo run -p nera-startup --example dump_startup`
//!
//! macOS: tab-separated records. `item` lines carry the parsed fields so they
//! can be compared with `plutil -p`, `launchctl print gui/$UID/<label>` and
//! `launchctl print-disabled gui/$UID`.
//!
//! Windows: `item` lines carry the registry / folder source, the raw
//! `StartupApproved` bytes and the parsed program, so they can be compared
//! with Task Manager > Startup apps, `reg query` and the Startup folders.

#[cfg(target_os = "macos")]
use nera_startup::engine::{self, Scope};
#[cfg(target_os = "macos")]
use nera_startup::view::{self, LoginState};
#[cfg(target_os = "macos")]
use std::time::Duration;

#[cfg(target_os = "macos")]
fn main() {
    let snapshot = engine::scan();
    let login = engine::read_login_items(Duration::from_secs(30));
    let state = match &login {
        Ok(items) => LoginState::Ready(items.len()),
        Err(_) => LoginState::Failed,
    };

    println!("subtitle\t{}", view::page_subtitle(Some(&snapshot)));
    for t in view::tiles(&snapshot, state, false) {
        println!("tile\t{}\t{}\t{}", t.label, t.value, t.caption);
    }
    for (dir, msg, perm) in &snapshot.folder_errors {
        println!("folder_error\t{}\t{msg}\tpermission={perm}", dir.display());
    }
    if let Some(e) = &snapshot.launchd_error {
        println!("launchd_error\t{e}");
    }

    match &login {
        Ok(items) => {
            println!("login_items\t{}", items.len());
            for i in items {
                println!(
                    "login\t{}\t{}\t{}",
                    i.name,
                    view::login_subtitle(i),
                    view::login_badge(i).unwrap_or("")
                );
            }
        }
        Err(e) => println!("login_error\t{e:?}"),
    }

    for item in &snapshot.items {
        let scope = match item.scope {
            Scope::User => "user",
            Scope::GlobalAgent => "global-agent",
            Scope::Daemon => "daemon",
        };
        let badges: Vec<&str> = view::agent_badges(item).iter().map(|b| b.label).collect();
        let info = item.info.as_ref();
        let (pid, exit) = match item.loaded {
            Some(Some(l)) => (
                l.pid.map(|p| p.to_string()).unwrap_or("-".into()),
                l.last_exit.map(|e| e.to_string()).unwrap_or("-".into()),
            ),
            Some(None) => ("notloaded".into(), "-".into()),
            None => ("unknown".into(), "-".into()),
        };
        println!(
            "item\t{scope}\t{}\t{}\tprogram={}\trun_at_load={}\tkeep_alive={}\tkeep_cond={}\tscheduled={}\twatch={}\tplist_disabled={}\toverride={:?}\tenabled={}\tpid={pid}\texit={exit}\tapple={}\tvendor={}\tmissing={}\teditable={}",
            item.plist.display(),
            item.label,
            item.executable().unwrap_or("-"),
            info.is_some_and(|i| i.run_at_load),
            info.is_some_and(|i| i.keep_alive),
            info.is_some_and(|i| i.keep_alive_conditional),
            info.is_some_and(|i| i.scheduled),
            info.is_some_and(|i| i.watches_files),
            info.is_some_and(|i| i.disabled),
            item.override_disabled,
            item.enabled(),
            item.is_apple,
            item.vendor,
            item.program_missing,
            item.is_editable(),
        );
        println!(
            "row\t{}\t{}\t[{}]",
            item.label,
            item.subtitle(),
            badges.join("] [")
        );
    }
}

#[cfg(windows)]
fn main() {
    use nera_startup::engine::{self, Scope, Source};
    use nera_startup::view;

    let snapshot = engine::scan();
    println!("subtitle\t{}", view::page_subtitle(Some(&snapshot)));
    for t in view::tiles(&snapshot) {
        println!("tile\t{}\t{}\t{}", t.label, t.value, t.caption);
    }
    for e in &snapshot.errors {
        println!("error\t{e}");
    }
    for (scope, title) in [
        (Scope::User, "Your startup apps"),
        (Scope::Machine, "Startup apps for all users"),
        (Scope::Store, "Microsoft Store apps"),
    ] {
        println!(
            "section\t{title}\t{}",
            view::section_description(scope, &snapshot)
        );
        let items: Vec<_> = snapshot.in_scope(scope).collect();
        if items.is_empty() {
            let (t, m) = view::empty_state(scope, false);
            println!("empty\t{t}\t{m}");
        }
        for item in items {
            let source = match item.source {
                Source::Run { hive, wow64 } => {
                    format!("{}-run{}", hive.label(), if wow64 { "32" } else { "" })
                }
                Source::Folder(hive) => format!("{}-folder", hive.label()),
                Source::Store => "store".to_string(),
            };
            let approved = item
                .approved
                .as_ref()
                .map(|b| {
                    b.iter()
                        .map(|x| format!("{x:02x}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_else(|| "none".into());
            println!(
                "item\t{source}\t{}\tstate={:?}\tapproved={approved}\tcommand={}\ttarget={}\tpublisher={}\tmissing={}\trunning={:?}\teditable={}\treason={}",
                item.entry,
                item.state,
                item.command,
                item.target
                    .as_ref()
                    .map(|t| t.display().to_string())
                    .unwrap_or("-".into()),
                item.publisher.as_deref().unwrap_or("-"),
                item.target_missing,
                item.running,
                item.is_editable(),
                item.read_only_reason().unwrap_or("-"),
            );
            let badges: Vec<&str> = view::badges(item).iter().map(|b| b.label).collect();
            println!(
                "row\t{}\t{}\t[{}]",
                item.name,
                view::subtitle(item),
                badges.join("] [")
            );
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn main() {
    eprintln!("dump_startup: this feature is available on macOS and Windows only");
}
