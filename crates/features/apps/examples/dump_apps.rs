//! Dev-only: print the Applications values exactly as the screen would show
//! them, for comparison with `ls`, `defaults`, `mdls`, `du`, `lsappinfo`, `pgrep`.
//! Read-only: nothing is moved or removed.
//!
//! cargo run -p nera-apps --example dump                              rows + tiles
//! cargo run -p nera-apps --example dump -- --leftovers "<App name>"  leftovers of one app
//!
//! On Windows it prints the rows and tiles for comparison with Settings > Apps >
//! Installed apps (and `reg query ...\Uninstall`). Nothing is launched there.

#[cfg(target_os = "macos")]
use nera_apps::engine::{self, Protection, RunningSet, SelfIdentity, SortKey};
#[cfg(any(target_os = "macos", windows))]
use nera_apps::view;
#[cfg(target_os = "macos")]
use nera_core::fs_util::Cancel;
#[cfg(target_os = "macos")]
use std::time::SystemTime;

#[cfg(windows)]
fn main() {
    use nera_apps::engine::{self, Protection, RunningSet, SelfIdentity, SortKey};
    let found = engine::discover(&SelfIdentity::current(), &RunningSet::snapshot());
    for scope in &found.unreadable {
        println!("unreadable\t{}", scope.tag());
    }
    let apps = found.apps;
    let st = view::stats(&apps);
    println!(
        "tile\tInstalled apps\t{}\t{}",
        st.apps_value, st.apps_caption
    );
    println!("tile\tSpace used\t{}\t{}", st.space_value, st.space_caption);
    println!(
        "tile\tRunning now\t{}\t{}",
        st.running_value, st.running_caption
    );
    let by_size = engine::filter_sort(&apps, "", SortKey::Size);
    println!("toolbar\t{}", view::toolbar_summary(&apps, &by_size));
    for i in engine::filter_sort(&apps, "", SortKey::Name) {
        let a = &apps[i];
        let mut badges: Vec<&str> = Vec::new();
        if a.running {
            badges.push("Running");
        }
        badges.extend(view::blocker_badge(a));
        println!(
            "row\t{}\t{}\t{}\t{}\tbadge={}\tself={}\tuninstall={}\tfolder={}",
            a.id,
            a.name,
            view::app_subtitle(a),
            view::app_size(a),
            badges.join("+"),
            a.protection == Protection::SelfApp,
            a.uninstall
                .as_ref()
                .map(|c| c.display())
                .unwrap_or_default(),
            a.app_folder
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        );
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn main() {
    eprintln!("dump_apps: this feature is available on macOS and Windows only");
}

#[cfg(target_os = "macos")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let running = RunningSet::snapshot();
    let found = engine::discover(&engine::default_roots(), &SelfIdentity::current(), &running);
    let mut apps = found.apps;
    for p in &found.unreadable {
        println!("unreadable\t{}", p.display());
    }
    let cancel = Cancel::new();
    for a in &mut apps {
        a.size = Some(engine::app_size(&a.path, &cancel));
    }
    let paths: Vec<_> = apps.iter().map(|a| a.path.clone()).collect();
    for (a, d) in apps.iter_mut().zip(engine::last_used_dates(&paths)) {
        a.last_used = d;
    }
    if args.first().map(String::as_str) == Some("--leftovers") {
        let name = args.get(1).expect("app name");
        let app = apps
            .iter()
            .find(|a| &a.name == name)
            .expect("no such app")
            .clone();
        let library = nera_core::paths::home_join("Library");
        let scan = engine::find_leftovers(
            &library,
            &engine::LeftoverQuery::for_app(&app, &apps),
            &cancel,
        );
        println!(
            "app\t{}\t{}\t{:?}",
            app.name,
            app.bundle_id,
            app.match_names()
        );
        let total: u64 = scan.items.iter().map(|l| l.size).sum();
        println!("header\t{}", view::leftover_header(scan.items.len(), total));
        for l in &scan.items {
            let parent = l
                .path
                .parent()
                .map(|p| {
                    p.display().to_string().replacen(
                        &nera_core::paths::home().display().to_string(),
                        "~",
                        1,
                    )
                })
                .unwrap_or_default();
            println!(
                "leftover\t{}\t{}\t{}\t{}",
                l.size,
                view::bytes(l.size),
                view::leftover_subtitle(l.kind, &parent),
                l.path.display()
            );
        }
        if scan.shared_skipped > 0 {
            println!("note\t{}", view::shared_note(scan.shared_skipped));
        }
        println!("denied\t{:?}", scan.denied);
        let f = view::footer(&app, false, scan.items.len(), total);
        println!("footer\t{}\t{}\t{}", f.summary, f.caption, f.button);
        return;
    }

    let now = SystemTime::now();
    let st = view::stats(&apps, false, now);
    println!(
        "tile\tRemovable apps\t{}\t{}",
        st.removable_value, st.removable_caption
    );
    println!("tile\tSpace used\t{}\t{}", st.space_value, st.space_caption);
    println!(
        "tile\tNot used in 6 months\t{}\t{}",
        st.unused_value, st.unused_caption
    );
    for show_apple in [false, true] {
        let visible = engine::filter_sort(&apps, "", SortKey::Size, show_apple, now);
        println!(
            "toolbar(show_apple={show_apple})\t{}",
            view::toolbar_summary(&apps, &visible, false)
        );
    }
    for i in engine::filter_sort(&apps, "", SortKey::Name, true, now) {
        let a = &apps[i];
        let badge = match (a.running, a.protection) {
            (_, Protection::SelfApp) => "This app",
            (true, Protection::Apple) => "Running+Apple",
            (true, _) => "Running",
            (false, Protection::Apple) => "Apple",
            _ => "",
        };
        println!(
            "row\t{}\t{}\t{}\t{}\tbid={}\tbytes={}\tbadge={}",
            a.path.display(),
            a.name,
            view::app_subtitle(a, false, now),
            view::app_size(a),
            a.bundle_id,
            a.size.unwrap_or(0),
            badge
        );
    }
}
