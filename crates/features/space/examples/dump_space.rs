//! Dev-only: print the Space Lens values exactly as the screen would show
//! them, for comparison with `du`, `find`, Finder and `system_profiler` on
//! macOS, File Explorer (folder Properties, "This PC") on Windows.
//!
//! cargo run -p nera-space --example dump_space -- [folder ...]            folder listings
//! cargo run -p nera-space --example dump_space -- --large <root> [bytes]  large files
//! cargo run -p nera-space --example dump_space -- --disk                  capacity tiles, root names

use nera_core::fs_util::Cancel;
use nera_space::{engine, view};
use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--disk") => disk(),
        Some("--large") => {
            let root = PathBuf::from(args.get(1).expect("root"));
            let threshold = args
                .get(2)
                .map(|s| s.parse().expect("bytes"))
                .unwrap_or(engine::LARGE_FILE_THRESHOLD);
            large(&root, threshold);
        }
        _ => {
            let dirs: Vec<PathBuf> = if args.is_empty() {
                vec![nera_core::paths::home()]
            } else {
                args.iter().map(PathBuf::from).collect()
            };
            for d in dirs {
                folder(&d);
            }
        }
    }
}

fn disk() {
    let cap = engine::startup_capacity();
    let o = view::overview(Some(&cap), "Home", "~", None, true);
    if let Ok(c) = &cap {
        println!(
            "raw total={} free(incl purgeable)={} purgeable={:?}",
            c.disk.total, c.disk.free, c.purgeable
        );
    }
    println!(
        "Used      {:>10}  {}  meter={:?}",
        o.used_value, o.used_caption, o.used_fraction
    );
    println!(
        "Available {:>10}  {}",
        o.available_value, o.available_caption
    );
    let home = nera_core::paths::home();
    println!(
        "root: {} = {}  on startup disk: {}",
        engine::folder_name(&home),
        engine::display_path(&home),
        engine::on_startup_disk(&home)
    );
    #[cfg(windows)]
    {
        let drive = nera_core::paths::system_drive();
        println!(
            "root: {} = {}  on startup disk: {}",
            engine::folder_name(&drive),
            engine::display_path(&drive),
            engine::on_startup_disk(&drive)
        );
    }
    println!("full disk access: {:?}", engine::has_full_disk_access());
}

fn folder(dir: &Path) {
    let listing = match engine::list_children(dir, &Cancel::new(), &|_| {}) {
        Ok(listing) => listing,
        Err(err) => {
            println!("== {}  error: {err}", engine::display_path(dir));
            return;
        }
    };
    let cap = engine::startup_capacity();
    let o = view::overview(
        Some(&cap),
        &engine::folder_name(dir),
        &engine::display_path(dir),
        Some(listing.total),
        listing.on_startup_disk,
    );
    println!(
        "== {}  summary: {}",
        engine::display_path(dir),
        view::folder_summary(&listing)
    );
    println!(
        "tile: {} = {} | {}",
        o.folder_title, o.folder_value, o.folder_caption
    );
    println!(
        "raw total bytes = {}  unreadable = {}",
        listing.total, listing.unreadable
    );
    for e in &listing.entries {
        println!(
            "{:>14}  {:>10}  {:>4}  {}  | {}",
            e.size,
            view::bytes(e.size),
            view::child_percent(&listing, e),
            e.path.display(),
            view::child_subtitle(e)
        );
    }
}

fn large(root: &Path, threshold: u64) {
    println!("{}", view::large_description(&engine::display_path(root)));
    let items =
        engine::find_large_files(root, threshold, 1000, &Cancel::new(), &|_| {}).expect("large");
    for f in &items {
        println!(
            "{:>14}  {:>10}  {}  | {}",
            f.size,
            view::bytes(f.size),
            f.path.display(),
            view::large_subtitle(f)
        );
    }
}
