//! Dev tool: prints every value the Performance screen shows, formatted by the
//! same helpers (`engine::present`), plus the raw numbers behind them, so they
//! can be compared with `top`, `vm_stat`, `sysctl`, `ioreg`, `df`, `uptime`
//! on macOS, and with Task Manager > Performance, `Get-Counter` and
//! `Get-CimInstance Win32_OperatingSystem` on Windows.
//!
//! `cargo run -p nera-performance --example dump_performance`
//!
//! Env: `DUMP_SAMPLES` (default 1) and `DUMP_INTERVAL_MS` (default 2000, the
//! screen's refresh period) repeat the sampling the way the screen does: each
//! sample's CPU figure covers the time since the previous one.

use nera_performance::engine::maintenance::{self, TaskId};
use nera_performance::engine::{present, stats, time, MaintenanceLog};
use std::time::Duration;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let samples = env_u64("DUMP_SAMPLES", 1);
    let interval = Duration::from_millis(env_u64("DUMP_INTERVAL_MS", 2000));
    let mut prev = None;
    for n in 0..samples {
        let snap = stats::sample(prev).expect("sample");
        prev = Some(snap.ticks);
        let now = time::now_unix();
        println!(
            "=== sample {n} at unix {now} ({})",
            time::date_time_label(now)
        );

        println!("-- Live status tiles");
        for t in present::tiles(&snap) {
            println!(
                "{:<16} | {:<12} | {}",
                t.title,
                t.value,
                t.caption.unwrap_or_default()
            );
        }

        println!("-- Graphics section");
        for g in &snap.gpus {
            for (label, value) in present::gpu_rows(g) {
                println!("{label:<24} | {value}");
            }
        }

        println!("-- Memory breakdown");
        for (label, value) in present::memory_rows(&snap.memory) {
            println!("{label:<24} | {value}");
        }

        println!("-- Raw");
        if let Some(c) = snap.cpu {
            println!(
                "cpu user {:.2}% system {:.2}% idle {:.2}% busy {:.2}%",
                c.user * 100.0,
                c.system * 100.0,
                c.idle * 100.0,
                c.busy() * 100.0
            );
        }
        print_raw(&snap);
        println!(
            "cores logical {} physical {} split {:?}; disk {:?}",
            snap.cores, snap.physical_cores, snap.core_split, snap.disk
        );
        for g in &snap.gpus {
            println!("gpu {g:?}");
        }

        if n + 1 == samples {
            println!("-- Maintenance rows (not run)");
            let log = MaintenanceLog::load();
            for id in maintenance::available_tasks() {
                let last = log.entry(id).and_then(|e| e.last_run);
                println!(
                    "{:<26} | {:<12} | admin={} | {}",
                    id.title(),
                    time::last_run_label(last, now),
                    id.needs_admin(),
                    id.description()
                );
            }
            let hidden: Vec<_> = TaskId::ALL
                .iter()
                .filter(|t| !t.is_available())
                .map(|t| t.title())
                .collect();
            println!("hidden (not offered on this system): {hidden:?}");
        } else {
            std::thread::sleep(interval);
        }
    }
}

#[cfg(target_os = "macos")]
fn print_raw(snap: &stats::Snapshot) {
    let vm = stats::read_vm_counts().expect("vm");
    println!("vm page_size={} {:?}", vm.page_size, vm);
    let m = snap.memory;
    println!(
        "memory bytes: total={} used={} app={} wired={} compressed={} cached={} free={}",
        m.total, m.used, m.app, m.wired, m.compressed, m.cached, m.free
    );
    println!(
        "pressure {:?}; swap {:?}; load {:?}; boot {:?}; uptime_secs {:?}",
        snap.pressure, snap.swap, snap.load, snap.boot_time, snap.uptime_secs
    );
}

#[cfg(windows)]
fn print_raw(snap: &stats::Snapshot) {
    let m = snap.memory;
    let s = stats::read_memory_status().expect("GlobalMemoryStatusEx");
    println!(
        "GlobalMemoryStatusEx: total_phys={} avail_phys={} total_page_file={} avail_page_file={}",
        s.total_phys, s.avail_phys, s.total_page_file, s.avail_page_file
    );
    println!(
        "memory bytes: total={} in_use={} available={} modified={:?} cached={:?} committed={} commit_limit={} paged_pool={:?} non_paged_pool={:?}",
        m.total,
        m.used,
        m.available,
        m.modified,
        m.cached,
        m.committed,
        m.commit_limit,
        m.paged_pool,
        m.non_paged_pool
    );
    println!(
        "boot {:?} ({}); uptime_secs {:?}; pressure/swap/load (macOS only) {:?} {:?} {:?}",
        snap.boot_time,
        snap.boot_time
            .map(time::date_time_label)
            .unwrap_or_default(),
        snap.uptime_secs,
        snap.pressure,
        snap.swap,
        snap.load
    );
    for a in nera_performance::engine::gpu::adapters() {
        println!(
            "adapter luid=0x{:x} gpu={} type=0b{:b} dedicated={} shared={} name={:?}",
            a.luid,
            a.is_gpu(),
            a.type_bits,
            a.dedicated_bytes,
            a.shared_bytes,
            a.name
        );
    }
}
