//! Read-only dump of everything the Privacy screen would show on this computer.
//! `cargo run -p nera-privacy --example dump_privacy`
//!
//! Lines starting with `#` are tab-separated records meant for comparing
//! against `du -s`, `ls`, `plutil` (macOS) or a folder's Properties in File
//! Explorer (Windows): `#target<TAB>group<TAB>kind<TAB>bytes<TAB>path`.

use nera_core::fs_util::Cancel;
use nera_privacy::engine::{self, Env};
use nera_privacy::view;
use std::collections::HashSet;

fn main() {
    let env = Env::real();
    let exes = engine::running_executables();
    let result = engine::scan(&env, &exes, &Cancel::new(), &|_| {});
    let to_trash = nera_core::settings::Settings::load().move_to_trash;
    let access = Some(result.full_disk_access);
    #[cfg(target_os = "macos")]
    println!("full_disk_access\t{:?}", result.full_disk_access);
    println!("move_to_trash\t{to_trash}");

    let none = HashSet::new();
    let sum = view::summary(&result.groups, &none, access).scan_stopped(result.cancelled);
    println!(
        "tile\tBrowser data\t{}\t{}",
        sum.browser_value(),
        sum.browser_caption()
    );
    #[cfg(target_os = "macos")]
    println!(
        "tile\tRecent items\t{}\tApple menu and Open Recent",
        sum.recent_value()
    );
    println!(
        "tile\tSelected\t{}\t{}",
        sum.selected_value(),
        sum.selected_caption()
    );
    println!(
        "footer\t{}\t{}",
        view::footer_summary(&sum),
        view::clean_button_label(&sum)
    );
    let running = view::running_names(&result);
    if !running.is_empty() {
        println!("banner\t{}", view::running_title(&running));
    }

    for g in &result.groups {
        let badge = if g.running {
            "  [Running — quit to clean]"
        } else {
            ""
        };
        println!(
            "section\t{}\t{}{badge}",
            g.name,
            view::group_description(g, access)
        );
        for i in &g.items {
            let state = view::row_state(g, i, access);
            let perm = if i.kind.permanent() && state.selectable() {
                " [Permanent]"
            } else {
                ""
            };
            println!(
                "row\t{}\t{}\t{}{perm}\t({:?}, {} bytes, {} targets)",
                i.id,
                i.kind.label(),
                view::row_value(i, access),
                state,
                i.bytes,
                i.targets.len()
            );
            for t in &i.targets {
                println!(
                    "#target\t{}\t{}\t{}\t{}",
                    g.id,
                    i.kind.key(),
                    t.bytes,
                    t.path.display()
                );
            }
        }
    }
    print_profiles(&env);
}

/// Profile lists, for comparison with Local State / profiles.ini.
#[cfg(target_os = "macos")]
fn print_profiles(env: &Env) {
    for (name, rel) in [
        ("chrome", "Library/Application Support/Google/Chrome"),
        ("edge", "Library/Application Support/Microsoft Edge"),
        (
            "brave",
            "Library/Application Support/BraveSoftware/Brave-Browser",
        ),
        ("arc", "Library/Application Support/Arc/User Data"),
        ("vivaldi", "Library/Application Support/Vivaldi"),
    ] {
        let profiles = engine::chromium_profiles(&env.home.join(rel));
        let list: Vec<String> = profiles
            .iter()
            .map(|p| format!("{} ({})", p.dir, p.name))
            .collect();
        println!("profiles\t{name}\t{}", list.join(", "));
    }
    let ff = engine::firefox_profiles(&env.home.join("Library/Application Support/Firefox"));
    let list: Vec<String> = ff.iter().map(|p| p.dir.display().to_string()).collect();
    println!("profiles\tfirefox\t{}", list.join(", "));
}

/// Profile lists, for comparison with Local State / profiles.ini.
#[cfg(windows)]
fn print_profiles(env: &Env) {
    for (name, rel) in [
        ("chrome", r"Google\Chrome\User Data"),
        ("edge", r"Microsoft\Edge\User Data"),
        ("brave", r"BraveSoftware\Brave-Browser\User Data"),
        ("vivaldi", r"Vivaldi\User Data"),
        ("chromium", r"Chromium\User Data"),
    ] {
        let profiles = engine::chromium_profiles(&env.local.join(rel));
        let list: Vec<String> = profiles
            .iter()
            .map(|p| format!("{} ({})", p.dir, p.name))
            .collect();
        println!("profiles\t{name}\t{}", list.join(", "));
    }
    let ff = engine::firefox_profiles(&env.roaming.join(r"Mozilla\Firefox"));
    let list: Vec<String> = ff.iter().map(|p| p.dir.display().to_string()).collect();
    println!("profiles\tfirefox\t{}", list.join(", "));
    let running: Vec<String> = exes_of_browsers(&engine::running_executables());
    println!("running_exes\t{}", running.join(", "));
}

/// Running executables that the engine treats as a browser, for checking
/// the running rule against Task Manager.
#[cfg(windows)]
fn exes_of_browsers(exes: &[std::path::PathBuf]) -> Vec<String> {
    let apps = [
        "chrome.exe",
        "msedge.exe",
        "firefox.exe",
        "brave.exe",
        "opera.exe",
        "vivaldi.exe",
        "Chromium",
    ];
    let mut out: Vec<String> = exes
        .iter()
        .filter(|exe| {
            apps.iter()
                .any(|app| engine::is_running(app, std::slice::from_ref(*exe)))
        })
        .map(|exe| exe.display().to_string())
        .collect();
    out.sort();
    out.dedup();
    out
}
