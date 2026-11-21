//! Dev-only: run the scan read-only and print every value the screen shows,
//! plus raw numbers for comparing with `du`, `stat` and `git` (macOS) or
//! File Explorer's "Size" (Windows).
//!
//!     cargo run -p nera-devscan --example dump_devscan -- [folder ...]
//!
//! With no arguments the default scan folders are used (like the screen).

use nera_core::fs_util::Cancel;
use nera_devscan::engine::{self, ScanOptions};
use nera_devscan::view;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn epoch(t: Option<SystemTime>) -> i64 {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(-1)
}

fn main() {
    let mut roots: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    if roots.is_empty() {
        roots = engine::default_roots(&nera_core::paths::home());
    }
    println!("OS\t{}", nera_core::os::NAME);
    println!(
        "ROOTS\t{}",
        roots
            .iter()
            .map(|r| engine::display_path(r))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let out = engine::scan(
        &ScanOptions {
            roots,
            excluded: Vec::new(),
        },
        &Cancel::new(),
        &|_| {},
    );
    let now = SystemTime::now();
    for t in [
        view::tile_size(&out),
        view::tile_projects(&out),
        view::tile_selected(0, 0, 0),
    ] {
        println!("TILE\t{}\t{}\t{}", t.0, t.1, t.2);
    }
    println!(
        "OUTCOME\tprojects={} junk={} total_bytes={} total_unique_bytes={} folders_scanned={} unreadable={} privacy_denied={} missing={} elapsed={:.1}s",
        out.projects.len(),
        out.junk_count(),
        out.total_size(),
        out.total_unique(),
        out.folders_scanned,
        out.unreadable_count,
        out.privacy_denied,
        out.missing_roots.len(),
        out.elapsed.as_secs_f64()
    );
    println!(
        "WORKTREES\trepos={} git_missing={}",
        out.worktrees.len(),
        out.worktrees_git_missing
    );
    for r in &out.worktrees {
        println!("WTREPO\t{}\t{}", r.name, r.repo.display());
        for w in &r.worktrees {
            println!(
                "WORKTREE\t{:?}\t{:?}\tselectable={} preselected={}\tbranch={:?}\tbytes={}\t{}\tpath={}",
                w.reason,
                w.reason.level(),
                view::worktree_selectable(w),
                view::worktree_preselected(w),
                w.branch,
                w.size,
                view::worktree_subtitle(w, now),
                w.path.display()
            );
        }
    }
    for p in &out.projects {
        println!(
            "PROJECT\t{}\t{}\t{}\t{}\tbadges={:?}\troot={}\tactivity_epoch={}\tbytes={}",
            p.name,
            nera_core::format::bytes(p.size()),
            view::project_subtitle(p, 0, now),
            p.junk.len(),
            view::project_badges(p, now),
            p.root.display(),
            epoch(p.last_activity),
            p.size()
        );
        for j in &p.junk {
            println!(
                "JUNK\t{}\t{}\t{}\t{}\teco={}\tpath={}\tmtime_epoch={}\tbytes={}\tunique={}",
                j.relative,
                nera_core::format::bytes(j.size),
                view::junk_subtitle(j, now),
                j.ecosystem.label(),
                p.primary().label(),
                j.path.display(),
                epoch(j.modified),
                j.size,
                j.unique
            );
        }
    }
}
