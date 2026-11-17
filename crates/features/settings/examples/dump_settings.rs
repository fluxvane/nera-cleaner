//! Read-only dump of everything the Settings screen would show on this computer.
//! `cargo run -p nera-settings --example dump`
//!
//! Compare with the persisted file (`settings.json` in the Nera Cleaner
//! Application Support folder), `du -s` of the cache folder, and
//! `Cargo.toml`'s workspace version.

use nera_core::format;
use nera_core::settings::Settings;
use nera_settings::{engine, view};

fn main() {
    let file = Settings::file_path();
    let s = Settings::load();
    println!(
        "settings_file\t{}\texists={}",
        file.display(),
        file.exists()
    );

    println!("theme\t{}", s.theme.label());
    println!(
        "cleanup_description\t{}",
        view::cleanup_description(s.move_to_trash)
    );
    println!(
        "move_to_trash\t{}\t{}",
        s.move_to_trash,
        view::MOVE_TO_TRASH_DESC
    );
    println!(
        "confirm_before_cleanup\t{}\t{}",
        s.confirm_before_cleanup,
        view::CONFIRM_DESC
    );
    println!(
        "notify_after_cleanup\t{}\t{}",
        s.notify_after_cleanup,
        view::NOTIFY_DESC
    );
    println!(
        "scan_hidden_files\t{}\t{}",
        s.scan_hidden_files,
        view::HIDDEN_DESC
    );
    println!(
        "max_scan_depth\t{}\t(stored {})\t{}",
        view::depth(&s),
        s.max_scan_depth,
        view::DEPTH_DESC
    );
    println!(
        "auto_scan_on_launch\t{}\t{}",
        s.auto_scan_on_launch,
        view::LAUNCH_DESC
    );
    if s.excluded_paths.is_empty() {
        println!("excluded\t(none) No excluded folders");
    }
    for p in &s.excluded_paths {
        let (title, subtitle) = view::excluded_row(p);
        println!("excluded\t{title}\t{subtitle}");
    }

    if engine::HAS_FULL_DISK_ACCESS {
        let report = engine::check_full_disk_access();
        let perm = view::permission(Some(&report), false);
        println!(
            "permission\t{}\t{:?}\t{}",
            perm.badge, perm.tone, perm.subtitle
        );
        println!(
            "permission_probe\t{:?}\treadable={:?}\tdenied={:?}",
            report.status, report.readable, report.denied
        );
    }

    let dir = engine::app_cache_dir();
    let bytes = engine::app_cache_size();
    println!(
        "cache\t{}\t{}\t#bytes={bytes}\t#path={}",
        engine::display_path(&dir),
        view::storage_value(Some(bytes)),
        dir.display()
    );
    println!("cache_formatted_check\t{}", format::bytes(bytes));
    println!("version\t{}", view::version_text());
    println!("copyright\t{}", view::COPYRIGHT);
}
