//! Dev-only: prints exactly what the Processes screen shows, using the same
//! engine and formatting helpers. `cargo run -p nera-processes --example dump [N]`
//! prints the summary tiles and, for each filter, the first N rows (default 15)
//! sorted by CPU and by memory, followed by a TSV of every process for
//! comparison with `ps`/`top` (macOS) or Task Manager's Details tab /
//! `Get-Counter '\Process(*)\Working Set - Private'` (Windows). A second
//! argument takes that many extra samples 2 s apart first, like the running
//! screen does, and prints the last one.

use nera_processes::engine::{self, Filter, Me, Sampler, SortKey};

fn main() -> anyhow::Result<()> {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(15);
    let me = Me::current();
    let mut sampler = Sampler::new();
    let extra: usize = std::env::args()
        .nth(2)
        .and_then(|a| a.parse().ok())
        .unwrap_or(0);
    let mut snap = sampler.sample()?;
    for _ in 0..extra {
        std::thread::sleep(std::time::Duration::from_secs(2));
        snap = sampler.sample()?;
    }
    let t = engine::stats_text(&snap, me.uid);
    println!("# tiles (uid {})", me.uid);
    println!("CPU load\t{}\t{}", t.cpu_value, t.cpu_caption);
    println!(
        "{}\t{}\t{}",
        engine::MEMORY_TILE,
        t.memory_value,
        t.memory_caption
    );
    println!("Processes\t{}\t{}", t.processes_value, t.processes_caption);
    println!("# apps_known={}", snap.apps_known);
    println!(
        "# raw load={} used={:?} total={} cores={}",
        snap.cpu_load, snap.memory_used, snap.memory_total, snap.cores
    );
    for filter in Filter::ALL {
        for sort in [SortKey::Cpu, SortKey::Memory, SortKey::Name] {
            let list = engine::visible(&snap.processes, filter, sort, "", me.uid);
            println!(
                "# filter={} sort={} count={}",
                filter.label(),
                sort.label(),
                list.len()
            );
            for p in list.iter().take(n) {
                let control = engine::control(p, &me);
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    p.name,
                    engine::subtitle(p),
                    engine::cpu_cell(p),
                    engine::memory_cell(p),
                    p.memory.unwrap_or(0),
                    match control {
                        engine::Control::Allowed => "quit".to_string(),
                        engine::Control::Caution(note) => format!("caution: {note}"),
                        engine::Control::Protected { badge, .. } => format!("[{badge}]"),
                    }
                );
            }
        }
    }
    println!("# all");
    for p in &snap.processes {
        println!(
            "ALL\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            p.pid,
            p.user,
            p.cpu.unwrap_or(-1.0),
            p.memory.unwrap_or(0),
            p.name,
            p.is_app(),
            p.parent.unwrap_or(-1)
        );
    }
    Ok(())
}
