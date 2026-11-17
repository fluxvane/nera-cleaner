//! Dev-only: run the duplicate scan read-only and print every value the
//! screen shows, plus raw numbers for checking with `shasum`, `stat`, `find`.
//!
//!     cargo run -p nera-duplicates --example dump_duplicates -- <folder> [--min BYTES] [--hidden]
//!     cargo run -p nera-duplicates --example dump_duplicates -- --fixture <new empty folder> [--min BYTES]
//!
//! `--fixture` first writes a small tree of known duplicates, near-duplicates,
//! hard links, clones, symlinks and empty files into `<folder>/scan` (and files
//! that share data with it into `<folder>/outside`), then scans `<folder>/scan`.
//! On Windows the fixture has junctions, HIDDEN and OFFLINE attributes and a
//! `$Recycle.Bin` folder instead of clones; check it with `Get-FileHash`,
//! `fsutil hardlink list` and File Explorer.
//! Nothing is ever deleted or moved.

use nera_core::format;
use nera_core::fs_util::Cancel;
use nera_duplicates::engine::{self, KeepRule, ScanOptions};
use nera_duplicates::view;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn put(root: &Path, rel: &str, data: &[u8]) -> PathBuf {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, data).unwrap();
    p
}

#[cfg(target_os = "macos")]
fn touch(p: &Path, stamp: &str) {
    let _ = Command::new("/usr/bin/touch")
        .args(["-t", stamp])
        .arg(p)
        .status();
}

#[cfg(target_os = "macos")]
fn fixture(root: &Path) {
    assert!(
        !root.exists() || std::fs::read_dir(root).unwrap().next().is_none(),
        "fixture folder must be new or empty"
    );
    let big = pattern(300_000, 3);
    touch(&put(root, "dup/a1.bin", &big), "202401010000");
    touch(&put(root, "dup/sub/a2.bin", &big), "202402020000");
    touch(&put(root, "dup/deep/er/a3.bin", &big), "202403030000");
    let small = pattern(5_000, 1);
    put(root, "small/s1.txt", &small);
    put(root, "small/s2.txt", &small);
    let near_a = pattern(300_000, 7);
    let mut near_b = near_a.clone();
    near_b[150_000] ^= 0xff;
    put(root, "near/n1.bin", &near_a);
    put(root, "near/n2.bin", &near_b);
    put(root, "same-size/x1.bin", &pattern(200_000, 20));
    put(root, "same-size/x2.bin", &pattern(200_000, 21));
    let h1 = put(root, "hard/h1.bin", &pattern(150_000, 30));
    std::fs::hard_link(&h1, root.join("hard/h2.bin")).unwrap();
    let c1 = put(root, "clone-only/c1.bin", &pattern(150_000, 40));
    let _ = Command::new("/bin/cp")
        .arg("-c")
        .arg(&c1)
        .arg(root.join("clone-only/c2.bin"))
        .status();
    let k1 = put(root, "clone-mixed/k1.bin", &pattern(250_000, 50));
    let _ = Command::new("/bin/cp")
        .arg("-c")
        .arg(&k1)
        .arg(root.join("clone-mixed/k2.bin"))
        .status();
    put(root, "clone-mixed/k3.bin", &pattern(250_000, 50));
    std::os::unix::fs::symlink(root.join("dup/a1.bin"), root.join("link-to-a1.bin")).unwrap();
    put(root, "zero/z1", b"");
    put(root, "zero/z2", b"");
    put(root, ".hid/a1-hidden.bin", &big);
    put(root, "Thing.app/Contents/a-in-app.bin", &big);
    put(root, "unicode/tài liệu.bin", &pattern(120_000, 60));
    put(root, "unicode/bản sao.bin", &pattern(120_000, 60));
    // Data shared with files outside the scanned folder: hard link + plain copy, and a clone + plain copy.
    let outside = root.parent().unwrap().join("outside");
    let o1 = put(&outside, "o1.bin", &pattern(180_000, 70));
    std::fs::create_dir_all(root.join("pinned")).unwrap();
    std::fs::hard_link(&o1, root.join("pinned/o1-link.bin")).unwrap();
    put(root, "pinned/o1-copy.bin", &pattern(180_000, 70));
    let o2 = put(&outside, "o2.bin", &pattern(160_000, 80));
    let _ = Command::new("/bin/cp")
        .arg("-c")
        .arg(&o2)
        .arg(root.join("pinned/o2-clone.bin"))
        .status();
    put(root, "pinned/o2-copy.bin", &pattern(160_000, 80));
    // Two copies that both live on outside the scan: nothing to free.
    let o3 = put(&outside, "o3.bin", &pattern(140_000, 90));
    let _ = Command::new("/bin/cp")
        .arg("-c")
        .arg(&o3)
        .arg(root.join("pinned/o3-a.bin"))
        .status();
    let _ = Command::new("/bin/cp")
        .arg("-c")
        .arg(&o3)
        .arg(root.join("pinned/o3-b.bin"))
        .status();
}

/// Set the modification time (seconds since 1970, UTC).
#[cfg(windows)]
fn stamp(p: &Path, unix_secs: u64) {
    let f = std::fs::File::options().write(true).open(p).unwrap();
    f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(unix_secs))
        .unwrap();
}

#[cfg(windows)]
fn add_attributes(p: &Path, attrs: u32) {
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::fs::MetadataExt as _;
    let wide: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
    // DIRECTORY (0x10) is not settable.
    let current = std::fs::symlink_metadata(p).unwrap().file_attributes() & !0x10;
    // SAFETY: NUL-terminated path.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::SetFileAttributesW(wide.as_ptr(), current | attrs)
    };
    assert_ne!(ok, 0, "SetFileAttributesW {}", p.display());
}

#[cfg(windows)]
fn fixture(root: &Path) {
    const HIDDEN: u32 = 0x2;
    const OFFLINE: u32 = 0x1000;
    assert!(
        !root.exists() || std::fs::read_dir(root).unwrap().next().is_none(),
        "fixture folder must be new or empty"
    );
    let big = pattern(300_000, 3);
    stamp(&put(root, "dup/a1.bin", &big), 1_704_067_200); // 2024-01-01
    stamp(&put(root, "dup/sub/a2.bin", &big), 1_706_832_000); // 2024-02-02
    stamp(&put(root, "dup/deep/er/a3.bin", &big), 1_709_424_000); // 2024-03-03
    let small = pattern(5_000, 1);
    put(root, "small/s1.txt", &small);
    put(root, "small/s2.txt", &small);
    let near_a = pattern(300_000, 7);
    let mut near_b = near_a.clone();
    near_b[150_000] ^= 0xff;
    put(root, "near/n1.bin", &near_a);
    put(root, "near/n2.bin", &near_b);
    put(root, "same-size/x1.bin", &pattern(200_000, 20));
    put(root, "same-size/x2.bin", &pattern(200_000, 21));
    let h1 = put(root, "hard/h1.bin", &pattern(150_000, 30));
    std::fs::hard_link(&h1, root.join("hard/h2.bin")).unwrap();
    // No clones on NTFS: two hard links plus an independent copy instead.
    let k1 = put(root, "hard-mixed/k1.bin", &pattern(250_000, 50));
    std::fs::hard_link(&k1, root.join("hard-mixed/k2.bin")).unwrap();
    put(root, "hard-mixed/k3.bin", &pattern(250_000, 50));
    // A junction to a folder holding another copy: never entered.
    let outside = root.parent().unwrap().join("outside");
    put(&outside, "junction-target/a-via-junction.bin", &big);
    // `mklink` only understands backslashes.
    let backslashed = |p: PathBuf| p.to_string_lossy().replace('/', "\\");
    let made = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(backslashed(root.join("junction")))
        .arg(backslashed(outside.join("junction-target")))
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(made, "mklink /J failed");
    // File symlink: needs Developer Mode or admin rights; skipped if made.
    let _ =
        std::os::windows::fs::symlink_file(root.join("dup/a1.bin"), root.join("link-to-a1.bin"));
    put(root, "zero/z1", b"");
    put(root, "zero/z2", b"");
    put(root, ".hid/a1-hidden.bin", &big);
    put(root, "hidattr/a1-hidden-attr.bin", &big);
    add_attributes(&root.join("hidattr"), HIDDEN);
    put(root, "$Recycle.Bin/a1-recycled.bin", &big);
    // Stand-in for an online-only OneDrive file (its attributes cannot be set by hand).
    add_attributes(&put(root, "cloud/a1-offline.bin", &big), OFFLINE);
    // An ordinary folder on Windows: its files are compared.
    put(root, "Thing.app/Contents/a-in-app.bin", &big);
    put(root, "unicode/tài liệu.bin", &pattern(120_000, 60));
    put(root, "unicode/bản sao.bin", &pattern(120_000, 60));
    // Data shared with files outside the scanned folder: hard link + plain copy.
    let o1 = put(&outside, "o1.bin", &pattern(180_000, 70));
    std::fs::create_dir_all(root.join("pinned")).unwrap();
    std::fs::hard_link(&o1, root.join("pinned/o1-link.bin")).unwrap();
    put(root, "pinned/o1-copy.bin", &pattern(180_000, 70));
    // Two links that both live on outside the scan: nothing to free.
    let o3 = put(&outside, "o3.bin", &pattern(140_000, 90));
    std::fs::hard_link(&o3, root.join("pinned/o3-a.bin")).unwrap();
    std::fs::hard_link(&o3, root.join("pinned/o3-b.bin")).unwrap();
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut root: Option<PathBuf> = None;
    let mut min = engine::DEFAULT_MIN_SIZE;
    let mut hidden = false;
    let mut make_fixture = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--fixture" => make_fixture = true,
            "--min" => {
                min = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--min BYTES")
            }
            "--hidden" => hidden = true,
            other => root = Some(PathBuf::from(other)),
        }
    }
    let mut root = root.expect("give a folder");
    if make_fixture {
        root = root.join("scan");
        fixture(&root);
    }
    let opts = ScanOptions {
        roots: vec![root],
        min_size: min,
        include_hidden: hidden,
        excluded: Vec::new(),
    };
    let report = engine::scan(&opts, &Cancel::new(), &|_| {});
    for f in engine::default_folders() {
        println!("DEFAULT_FOLDER\t{}", view::tilde(&f));
    }
    println!("MIN\t{} ({} bytes)", view::threshold(min), min);
    for t in [
        view::tile_reclaimable(&report),
        view::tile_sets(&report),
        view::tile_extra(&report),
        view::tile_compared(&report, min),
    ] {
        println!("TILE\t{}\t{}\t{}", t.0, t.1, t.2);
    }
    println!("EMPTY_MESSAGE\t{}", view::empty_message(&report, min));
    println!(
        "REPORT\tgroups={} wasted_bytes={} extra={} files_considered={} folders_searched={} shared_sets={} unreadable={} denied={}",
        report.groups.len(),
        report.wasted(),
        report.extra_copies(),
        report.files_considered,
        report.folders_searched,
        report.shared_sets,
        report.unreadable,
        report.denied.len()
    );
    for rule in KeepRule::ALL {
        let sel = engine::auto_select(&report.groups, rule);
        println!(
            "FOOTER\t{}\t{}",
            rule.label(),
            view::footer_summary(
                sel.len(),
                engine::selected_size(&report.groups, &sel),
                engine::selection_is_upper_bound(&report.groups, &sel)
            )
        );
        // Never every copy of a set.
        for g in &report.groups {
            assert!(
                g.files.iter().any(|f| !sel.contains(&f.path)),
                "rule {rule:?} selects every copy of {}",
                g.id
            );
        }
    }
    let keep_rule = KeepRule::Newest;
    let sel: HashSet<PathBuf> = engine::auto_select(&report.groups, keep_rule);
    for g in &report.groups {
        println!(
            "GROUP\t{}\tsize={}\tcopies={}\tfamilies={}\twasted={}",
            view::group_label(g),
            g.size,
            g.files.len(),
            g.families(),
            g.wasted()
        );
        let keeper = engine::keeper(g, keep_rule).map(|f| f.path.clone());
        for f in &g.files {
            let badge = if sel.contains(&f.path) {
                "SELECTED"
            } else if keeper.as_ref() == Some(&f.path) {
                "Keep(keeper)"
            } else {
                "Keep"
            };
            println!(
                "FILE\t{}\t{}\t{}\t{}\tshares_space={}\tpath={}",
                f.name(),
                format::bytes(f.size),
                view::file_subtitle(f),
                badge,
                g.shares_data(f),
                f.path.display()
            );
        }
    }
}
