//! Dev-only: prints every value the Dashboard and System Junk screens show,
//! computed by the same engine and presentation functions, so it can be
//! compared with `du`, `df`, `diskutil` and friends (on Windows: File
//! Explorer's This PC and folder Properties). Read-only: it scans, it never
//! cleans, and it creates nothing.
//!
//!   cargo run -p nera-cleanup --example dump_cleanup            screen values
//!   cargo run -p nera-cleanup --example dump_cleanup -- items   + one line per removable item
//!   cargo run -p nera-cleanup --example dump_cleanup -- overlap + items nested in items of other categories
//!   cargo run -p nera-cleanup --example dump_cleanup -- disk            only the disk tile (fast)
//!   cargo run -p nera-cleanup --example dump_cleanup -- contents <dir>...  bytes freed by emptying each folder (as a System Junk target)
//!   cargo run -p nera-cleanup --example dump_cleanup -- size <path>...   bytes freed by removing each path (permanent / trash)

use nera_cleanup::engine::{self, Category, Disposal, LastCleanup, Options, Scanned};
use nera_cleanup::present;
use nera_cleanup::ScanResults;
use nera_core::fs_util::Cancel;
use nera_core::settings::Settings;
use nera_core::{disk, paths};
use std::collections::HashSet;
use std::path::PathBuf;

/// Settings as the app reads them, without creating the support folder.
fn settings() -> Settings {
    let file = paths::app_support_dir().join("settings.json");
    std::fs::read(file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn last_cleanup() -> Option<LastCleanup> {
    LastCleanup::load_from(&paths::app_support_dir().join("last-cleanup.json"))
}

fn print_tile(t: &present::Tile) {
    println!("TILE\t{}\t{}\t{}", t.title, t.value, t.caption);
}

/// The disk tile with the raw numbers behind it (`statfs`, what `df` shows;
/// `GetDiskFreeSpaceExW` on Windows).
fn print_disk() {
    let d = disk::startup_volume().ok().map(|v| v.space);
    if let Some(d) = d {
        println!(
            "DISKRAW\ttotal={}\tfree={}\tused={}",
            d.total,
            d.free,
            d.used()
        );
    }
    print_tile(&present::disk_tile(d));
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    if mode == "disk" {
        print_disk();
        return;
    }
    if mode == "contents" {
        let s = settings();
        let opts = Options {
            include_hidden: s.scan_hidden_files,
            to_trash: s.move_to_trash,
            ..Default::default()
        };
        for p in std::env::args().skip(2) {
            let category = Category {
                id: "probe",
                name: "probe",
                description: "",
                icon: "folder",
                group: engine::Group::Caches,
                paths: vec![PathBuf::from(&p)],
                safe: true,
                mode: engine::Mode::Contents,
            };
            let out = engine::scan(vec![category], &opts, &Cancel::new(), |_| {});
            println!("CONTENTS\t{}\tdenied={:?}\t{}", out.total(), out.denied, p);
        }
        return;
    }
    if mode == "size" {
        for p in std::env::args().skip(2) {
            let path = PathBuf::from(&p);
            let perm = engine::item_bytes(&path, engine::Removal::Permanent, &Cancel::new());
            let trash = engine::item_bytes(&path, engine::Removal::Trash, &Cancel::new());
            println!("SIZE\t{perm}\t{trash}\t{p}");
        }
        return;
    }
    let s = settings();
    let opts = Options {
        include_hidden: s.scan_hidden_files,
        max_depth: s.scan_depth(),
        to_trash: s.move_to_trash,
        excluded: s.excluded_paths.clone(),
        reserved: engine::reserved_paths(),
    };
    println!(
        "SETTINGS\thidden={}\tto_trash={}\texcluded={:?}",
        opts.include_hidden, opts.to_trash, opts.excluded
    );

    print_disk();

    let categories = engine::all_categories();
    let all = categories.clone();
    let outcome = engine::scan(categories, &opts, &Cancel::new(), |_| {});
    let fda = engine::full_disk_access();
    let results = ScanResults {
        categories: outcome.categories.clone(),
        denied: outcome.denied.clone(),
        full_disk_access: fda,
        scanned_at: engine::unix_now(),
        elapsed: outcome.elapsed,
    };
    // Cloud files stored on this disk (shown, never cleaned).
    for d in engine::cloud_drives(&Cancel::new()) {
        println!(
            "CLOUD\t{}\t{}\t{}\t{}\tshown={}",
            d.service,
            present::cloud_subtitle(&d),
            present::bytes(d.local_bytes),
            d.local_bytes,
            d.local_bytes >= present::CLOUD_DRIVE_MIN
        );
    }
    // Data of system features (shown, never cleaned).
    for a in engine::system_assets(&Cancel::new()) {
        println!(
            "SYSTEM\t{}\t{}\t{}\t{}\tshown={}",
            a.feature.name(),
            a.feature.detail(),
            present::bytes(a.bytes),
            a.bytes,
            a.bytes >= present::SYSTEM_ASSET_MIN
        );
    }
    let now = engine::unix_now();
    println!(
        "FDA\t{:?}\tdenied={:?}\tbanner={}",
        fda,
        results.denied,
        results.needs_full_disk_access()
    );

    print_tile(&present::junk_tile(
        Some(&results),
        nera_cleanup::Phase::Idle,
    ));
    print_tile(&present::last_cleanup_tile(last_cleanup(), now));
    let summary = present::dashboard_summary(&results, now);
    println!("SUMMARY\t{}", summary.description);
    if let Some(b) = &summary.body {
        println!("SUMMARY\t{}\t{}", b.headline, b.detail);
        for (name, size) in &b.top {
            println!("TOP\t{name}\t{size}");
        }
    }
    println!("TOOLBAR\t{}", present::toolbar_summary(&results, now));
    for g in present::groups(&results.categories) {
        println!("GROUP\t{}\t{}", g.label, g.total);
        for r in &g.rows {
            let raw = results
                .categories
                .iter()
                .find(|c| c.category.id == r.id)
                .map_or(0, |c| c.size);
            println!(
                "ROW\t{}\t{}\t{}\t{}\t{:?}\t{}",
                r.id, r.name, r.value, raw, r.badges, r.description
            );
        }
    }
    for n in present::notes(&results) {
        println!("NOTE\t{n}");
    }

    for (label, selection) in [
        ("auto", engine::auto_selection(&results.categories)),
        ("safe", engine::safe_selection(&results.categories)),
    ] {
        let (count, bytes) = present::selection_totals(&results.categories, &selection);
        let picked: Vec<&Category> = results
            .categories
            .iter()
            .filter(|c| c.size > 0 && selection.contains(c.category.id))
            .map(|c| &c.category)
            .collect();
        let disposal = Disposal::of(&picked, s.move_to_trash);
        let f = present::footer(count, bytes, &disposal);
        let review: Vec<&str> = picked.iter().filter(|c| !c.safe).map(|c| c.name).collect();
        let (title, message) = present::confirm_text(count, bytes, &disposal, &review);
        println!(
            "FOOTER\t{label}\t{}\t{}\t{}\t(raw bytes {bytes})",
            f.headline, f.hint, f.button
        );
        println!("CONFIRM\t{label}\t{title}\t{message}");
    }
    println!(
        "RAWTOTAL\t{}\tsafe={}\tscan_seconds={:.1}",
        results.total(),
        results.safe_total(),
        outcome.elapsed.as_secs_f64()
    );

    if mode == "items" || mode == "overlap" {
        let index = opts.index(&all);
        let mut seen: Vec<(&'static str, PathBuf)> = Vec::new();
        for Scanned { category, size, .. } in &outcome.categories {
            println!("CAT\t{}\t{}\t{}", category.id, size, present::bytes(*size));
            for path in &category.paths {
                println!("PATH\t{}\t{}", category.id, path.display());
                let skip = index.descendant_exclusions(path, category.id);
                for (item, removal) in engine::removal_items(category, path, &skip, &opts) {
                    let bytes = engine::item_bytes(&item, removal, &Cancel::new());
                    println!(
                        "ITEM\t{}\t{:?}\t{}\t{}",
                        category.id,
                        removal,
                        bytes,
                        item.display()
                    );
                    seen.push((category.id, paths::standardize(&item)));
                }
            }
        }
        if mode == "overlap" {
            let mut dup: HashSet<String> = HashSet::new();
            for (i, (ca, a)) in seen.iter().enumerate() {
                for (cb, b) in seen.iter().skip(i + 1) {
                    if ca != cb && (a.starts_with(b) || b.starts_with(a)) {
                        dup.insert(format!("{ca}:{} <-> {cb}:{}", a.display(), b.display()));
                    }
                }
            }
            println!("OVERLAP\t{} pairs", dup.len());
            for d in dup {
                println!("OVERLAP\t{d}");
            }
        }
    }
}
