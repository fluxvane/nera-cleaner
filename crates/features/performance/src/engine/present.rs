//! Everything the Performance screen prints, as pure functions of the engine
//! values. The screen builds its tiles and rows from these and the
//! `dump` example prints the same strings, so what was verified against
//! Apple's tools is exactly what the UI shows.
//!
//! Units follow the tool a user would compare with: memory, swap and GPU
//! memory are binary (1 GB = 1024^3 bytes, the numbers Activity Monitor and
//! About This Mac show, with the same GB labels); disk
//! capacity is decimal like Finder (`nera_core::format::bytes`).
//!
//! On Windows the tiles and rows follow Task Manager's Performance page
//! ("In use", "Committed", ...) and disk capacity is binary like File
//! Explorer ("476 GB free of 931 GB" is GiB there too).

use super::disk::StartupVolume;
use super::gpu::GpuStats;
use super::stats::{CpuLoad, MemoryStats, Pressure, Snapshot, SwapUsage};
use super::time;
use nera_core::format;

/// Text of one overview tile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileText {
    pub title: &'static str,
    pub value: String,
    pub caption: Option<String>,
}

impl TileText {
    fn new(title: &'static str, value: impl Into<String>, caption: Option<String>) -> Self {
        Self {
            title,
            value: value.into(),
            caption,
        }
    }
}

/// Bytes in binary units (KB / MB / GB / TB), e.g. "49.47 GB". A value
/// that rounds up to the next unit is shown in that unit ("1.00 GB", never
/// "1024.0 MB").
pub fn memory_bytes(n: u64) -> String {
    format::memory_bytes(n)
}

/// CPU split as whole percentages that always add up to 100, so the three
/// numbers on screen never contradict each other (largest-remainder rounding).
/// Returns `(user, system, idle)`.
pub fn cpu_percents(c: &CpuLoad) -> (u32, u32, u32) {
    let parts = [c.user.max(0.0), c.system.max(0.0), c.idle.max(0.0)];
    let sum: f64 = parts.iter().sum();
    if sum <= 0.0 {
        return (0, 0, 100);
    }
    let exact: Vec<f64> = parts.iter().map(|p| p / sum * 100.0).collect();
    let mut out = [0u32; 3];
    for (o, e) in out.iter_mut().zip(&exact) {
        *o = e.floor() as u32;
    }
    let mut missing = 100 - out.iter().sum::<u32>();
    let mut order = [0usize, 1, 2];
    order.sort_by(|&a, &b| (exact[b] - exact[b].floor()).total_cmp(&(exact[a] - exact[a].floor())));
    for i in order {
        if missing == 0 {
            break;
        }
        out[i] += 1;
        missing -= 1;
    }
    (out[0], out[1], out[2])
}

pub fn cpu(c: Option<&CpuLoad>) -> TileText {
    match c {
        Some(c) => {
            let (user, system, idle) = cpu_percents(c);
            TileText::new(
                "CPU",
                format!("{}%", user + system),
                Some(format!("User {user}% · System {system}% · Idle {idle}%")),
            )
        }
        None => TileText::new("CPU", "…", Some("Measuring".into())),
    }
}

pub fn memory(m: &MemoryStats) -> TileText {
    TileText::new(
        "Memory",
        memory_bytes(m.used),
        Some(format!(
            "of {} · {} used",
            memory_bytes(m.total),
            format::percent(m.used_fraction())
        )),
    )
}

pub fn swap(s: Option<&SwapUsage>) -> TileText {
    let Some(s) = s else {
        return TileText::new("Swap", "Unknown", None);
    };
    if s.total == 0 {
        TileText::new("Swap", memory_bytes(0), Some("Swap not in use".into()))
    } else {
        TileText::new(
            "Swap",
            memory_bytes(s.used),
            Some(format!("of {} allocated", memory_bytes(s.total))),
        )
    }
}

pub fn uptime(secs: Option<u64>, boot_time: Option<i64>) -> TileText {
    match secs {
        Some(secs) => TileText::new(
            "Uptime",
            time::uptime_label(secs),
            boot_time.map(|b| format!("Since {}", time::date_time_label(b))),
        ),
        None => TileText::new("Uptime", "Unknown", None),
    }
}

/// "10 cores (8P + 2E)", "6 cores · 12 threads" (hyper-threading) or "10 cores".
/// `logical` is `hw.ncpu`, `physical` is `hw.physicalcpu`; the P/E split is only
/// shown when it accounts for every logical CPU.
pub fn cores_label(logical: usize, physical: usize, split: Option<(usize, usize)>) -> String {
    match split {
        Some((p, e)) if p + e == logical => format!("{logical} cores ({p}P + {e}E)"),
        _ if physical > 0 && physical < logical => {
            format!(
                "{} · {}",
                format::count(physical, "core"),
                format::count(logical, "thread")
            )
        }
        _ => format::count(logical, "core"),
    }
}

pub fn load(
    load: Option<[f64; 3]>,
    cores: usize,
    physical: usize,
    split: Option<(usize, usize)>,
) -> TileText {
    let cores = cores_label(cores, physical, split);
    match load {
        Some([a, b, c]) => TileText::new(
            "Load average",
            format!("{a:.2}"),
            Some(format!("1 min · 5 min {b:.2} · 15 min {c:.2} · {cores}")),
        ),
        None => TileText::new("Load average", "Unknown", Some(cores)),
    }
}

pub fn pressure(p: Option<Pressure>) -> TileText {
    match p {
        Some(p) => TileText::new(
            "Memory pressure",
            p.label(),
            Some("How hard macOS works to free memory".into()),
        ),
        None => TileText::new("Memory pressure", "Unknown", None),
    }
}

/// Startup disk. The caption says whether purgeable space is counted.
#[cfg(target_os = "macos")]
pub fn disk(d: Option<&StartupVolume>) -> TileText {
    match d {
        Some(d) => {
            let total = format::bytes(d.space.total);
            let caption = if d.includes_purgeable {
                format!("available of {total}")
            } else {
                format!("free of {total} · excludes purgeable")
            };
            TileText::new("Startup disk", format::bytes(d.space.free), Some(caption))
        }
        None => TileText::new("Startup disk", "Unknown", None),
    }
}

/// System drive, with File Explorer's binary units and wording ("free of").
#[cfg(windows)]
pub fn disk(d: Option<&StartupVolume>) -> TileText {
    match d {
        Some(d) => {
            let drive = nera_core::paths::system_drive();
            let drive = drive.to_string_lossy();
            TileText::new(
                "System drive",
                memory_bytes(d.space.free),
                Some(format!(
                    "free of {} · {}",
                    memory_bytes(d.space.total),
                    drive.trim_end_matches('\\')
                )),
            )
        }
        None => TileText::new("System drive", "Unknown", None),
    }
}

/// Commit charge, as Task Manager's "Committed" (physical memory plus page
/// files; Windows has no separate swap figure).
#[cfg(windows)]
pub fn committed(m: &MemoryStats) -> TileText {
    TileText::new(
        "Committed",
        memory_bytes(m.committed),
        Some(format!(
            "of {} commit limit · {}",
            memory_bytes(m.commit_limit),
            format::percent(m.commit_fraction())
        )),
    )
}

/// The busiest GPU (Macs with two GPUs show the active one).
pub fn busiest_gpu(gpus: &[GpuStats]) -> Option<&GpuStats> {
    gpus.iter()
        .max_by(|a, b| a.utilization.total_cmp(&b.utilization))
}

pub fn gpu(gpus: &[GpuStats]) -> TileText {
    match busiest_gpu(gpus) {
        Some(g) => {
            let caption = match (gpus.len(), g.memory_used) {
                (n, _) if n > 1 => format!("{} · busiest of {n} GPUs", g.name),
                (_, Some(mem)) => format!("{} · {} memory in use", g.name, memory_bytes(mem)),
                _ => g.name.clone(),
            };
            TileText::new("GPU", format::percent(g.utilization), Some(caption))
        }
        None => TileText::new(
            "GPU",
            "Unavailable",
            Some("No GPU statistics reported".into()),
        ),
    }
}

/// Label/value rows of the "Memory breakdown" card.
#[cfg(target_os = "macos")]
pub fn memory_rows(m: &MemoryStats) -> Vec<(&'static str, String)> {
    vec![
        ("App memory", memory_bytes(m.app)),
        ("Wired", memory_bytes(m.wired)),
        ("Compressed", memory_bytes(m.compressed)),
        ("Cached files", memory_bytes(m.cached)),
        ("Free", memory_bytes(m.free)),
        ("Physical memory", memory_bytes(m.total)),
    ]
}

/// Label/value rows of the "Memory breakdown" card, named as in Task
/// Manager. Rows Windows did not report are left out.
#[cfg(windows)]
pub fn memory_rows(m: &MemoryStats) -> Vec<(&'static str, String)> {
    let mut rows = vec![
        ("In use", memory_bytes(m.used)),
        ("Available", memory_bytes(m.available)),
    ];
    if let Some(c) = m.cached {
        rows.push(("Cached", memory_bytes(c)));
    }
    if let Some(p) = m.paged_pool {
        rows.push(("Paged pool", memory_bytes(p)));
    }
    if let Some(p) = m.non_paged_pool {
        rows.push(("Non-paged pool", memory_bytes(p)));
    }
    rows.push((
        "Committed",
        format!(
            "{} of {}",
            memory_bytes(m.committed),
            memory_bytes(m.commit_limit)
        ),
    ));
    rows.push(("Usable physical memory", memory_bytes(m.total)));
    rows
}

/// Label/value rows of one GPU card in the "Graphics" section (the first row
/// carries the GPU name and overall load).
pub fn gpu_rows(g: &GpuStats) -> Vec<(String, String)> {
    let mut rows = vec![(g.name.clone(), format::percent(g.utilization))];
    if let Some(r) = g.renderer {
        rows.push(("Renderer (shading)".into(), format::percent(r)));
    }
    if let Some(t) = g.tiler {
        rows.push(("Tiler (geometry)".into(), format::percent(t)));
    }
    if let Some(used) = g.memory_used {
        rows.push(("Graphics memory in use".into(), memory_bytes(used)));
    }
    if let Some(total) = g.memory_allocated {
        let label = if g.memory_is_vram {
            "Video memory (VRAM)"
        } else {
            "Graphics memory allocated"
        };
        rows.push((label.into(), memory_bytes(total)));
    }
    rows
}

/// The eight overview tiles in screen order.
#[cfg(target_os = "macos")]
pub fn tiles(s: &Snapshot) -> Vec<TileText> {
    vec![
        cpu(s.cpu.as_ref()),
        gpu(&s.gpus),
        memory(&s.memory),
        disk(s.disk.as_ref()),
        swap(s.swap.as_ref()),
        pressure(s.pressure),
        load(s.load, s.cores, s.physical_cores, s.core_split),
        uptime(s.uptime_secs, s.boot_time),
    ]
}

/// The overview tiles in screen order. Swap, memory pressure and load average
/// do not exist on Windows; the GPU tile is left out when Windows reports no
/// GPU counters.
#[cfg(windows)]
pub fn tiles(s: &Snapshot) -> Vec<TileText> {
    let mut tiles = vec![cpu(s.cpu.as_ref())];
    if !s.gpus.is_empty() {
        tiles.push(gpu(&s.gpus));
    }
    tiles.extend([
        memory(&s.memory),
        disk(s.disk.as_ref()),
        committed(&s.memory),
        uptime(s.uptime_secs, s.boot_time),
    ]);
    tiles
}

#[cfg(test)]
mod tests {
    use super::*;
    use nera_core::disk::DiskSpace;

    #[test]
    fn memory_units_are_binary() {
        assert_eq!(memory_bytes(0), "0 bytes");
        assert_eq!(memory_bytes(1), "1 byte");
        assert_eq!(memory_bytes(1023), "1023 bytes");
        assert_eq!(memory_bytes(1024), "1 KB");
        assert_eq!(memory_bytes(5 * 1024 * 1024 + 512 * 1024), "5.5 MB");
        assert_eq!(memory_bytes(68_719_476_736), "64.00 GB");
        assert_eq!(memory_bytes(300 * 1024 * 1024), "300.0 MB");
    }

    #[test]
    fn memory_units_carry_instead_of_printing_1024() {
        assert_eq!(memory_bytes(1024 * 1024 - 1), "1.0 MB");
        assert_eq!(memory_bytes(1024 * 1024 * 1024 - 1), "1.00 GB");
        assert_eq!(memory_bytes(1024 * 1024 * 1024 - 60_000), "1023.9 MB");
        assert_eq!(memory_bytes(1 << 40), "1.00 TB");
        assert_eq!(memory_bytes((1 << 40) - 1), "1.00 TB");
    }

    #[test]
    fn cpu_percentages_always_total_100() {
        let c = CpuLoad {
            user: 0.115,
            system: 0.044,
            idle: 0.841,
        };
        let (u, s, i) = cpu_percents(&c);
        assert_eq!(u + s + i, 100);
        assert_eq!((u, s, i), (12, 4, 84));
        // three thirds
        let third = CpuLoad {
            user: 1.0 / 3.0,
            system: 1.0 / 3.0,
            idle: 1.0 / 3.0,
        };
        let (u, s, i) = cpu_percents(&third);
        assert_eq!(u + s + i, 100);
        assert_eq!(cpu_percents(&CpuLoad::default()), (0, 0, 100));
        let t = cpu(Some(&c));
        assert_eq!(t.value, "16%");
        assert_eq!(t.caption.unwrap(), "User 12% · System 4% · Idle 84%");
        assert_eq!(cpu(None).value, "…");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn memory_tile_and_rows() {
        let m = MemoryStats {
            total: 64 << 30,
            used: 32 << 30,
            app: 20 << 30,
            wired: 5 << 30,
            compressed: 7 << 30,
            cached: 16 << 30,
            free: 1 << 30,
        };
        let t = memory(&m);
        assert_eq!(t.value, "32.00 GB");
        assert_eq!(t.caption.unwrap(), "of 64.00 GB · 50% used");
        let rows = memory_rows(&m);
        assert_eq!(rows[0], ("App memory", "20.00 GB".to_string()));
        assert_eq!(rows[5], ("Physical memory", "64.00 GB".to_string()));
    }

    #[test]
    fn swap_and_load_and_uptime() {
        assert_eq!(
            swap(Some(&SwapUsage::default())).caption.unwrap(),
            "Swap not in use"
        );
        let s = swap(Some(&SwapUsage {
            total: 2 << 30,
            used: 1 << 29,
            encrypted: true,
        }));
        assert_eq!(
            (s.value.as_str(), s.caption.unwrap().as_str()),
            ("512.0 MB", "of 2.00 GB allocated")
        );
        let l = load(Some([1.234, 2.0, 3.456]), 10, 10, Some((8, 2)));
        assert_eq!(l.value, "1.23");
        assert_eq!(
            l.caption.unwrap(),
            "1 min · 5 min 2.00 · 15 min 3.46 · 10 cores (8P + 2E)"
        );
        assert_eq!(load(None, 1, 1, None).caption.unwrap(), "1 core");
        assert_eq!(swap(None).value, "Unknown");
        assert_eq!(pressure(None).value, "Unknown");
        assert_eq!(pressure(Some(Pressure::Warning)).value, "Elevated");
        assert_eq!(uptime(None, Some(1)).value, "Unknown");
        assert_eq!(uptime(Some(3_725), None).caption, None);
    }

    #[test]
    fn core_labels() {
        assert_eq!(cores_label(10, 10, Some((8, 2))), "10 cores (8P + 2E)");
        // split that does not add up to every logical CPU is not shown
        assert_eq!(cores_label(12, 12, Some((8, 2))), "12 cores");
        // hyper-threaded Intel
        assert_eq!(cores_label(12, 6, None), "6 cores · 12 threads");
        assert_eq!(cores_label(8, 8, None), "8 cores");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn disk_caption_names_what_is_counted() {
        let space = DiskSpace {
            total: 994_662_584_320,
            free: 29_928_210_432,
        };
        let t = disk(Some(&StartupVolume {
            space,
            includes_purgeable: true,
        }));
        assert_eq!(t.value, "29.9 GB");
        assert_eq!(t.caption.unwrap(), "available of 995 GB");
        let t = disk(Some(&StartupVolume {
            space,
            includes_purgeable: false,
        }));
        assert_eq!(t.caption.unwrap(), "free of 995 GB · excludes purgeable");
        assert_eq!(disk(None).value, "Unknown");
    }

    #[test]
    fn gpu_text() {
        let g = GpuStats {
            name: "Apple M1 Max".into(),
            utilization: 0.18,
            renderer: Some(0.17),
            tiler: Some(0.12),
            memory_used: Some(300 << 20),
            memory_allocated: Some(4 << 30),
            memory_is_vram: false,
        };
        let t = gpu(std::slice::from_ref(&g));
        assert_eq!(t.value, "18%");
        assert_eq!(t.caption.unwrap(), "Apple M1 Max · 300.0 MB memory in use");
        let rows = gpu_rows(&g);
        assert_eq!(rows[0], ("Apple M1 Max".to_string(), "18%".to_string()));
        assert_eq!(
            rows[3],
            ("Graphics memory in use".to_string(), "300.0 MB".to_string())
        );
        assert_eq!(
            rows[4],
            (
                "Graphics memory allocated".to_string(),
                "4.00 GB".to_string()
            )
        );
        let vram = gpu_rows(&GpuStats {
            memory_is_vram: true,
            ..g.clone()
        });
        assert_eq!(vram[4].0, "Video memory (VRAM)");
        let two = [
            g.clone(),
            GpuStats {
                utilization: 0.5,
                name: "AMD".into(),
                ..Default::default()
            },
        ];
        assert_eq!(gpu(&two).caption.unwrap(), "AMD · busiest of 2 GPUs");
        assert_eq!(gpu(&[]).value, "Unavailable");
    }

    #[cfg(windows)]
    #[test]
    fn windows_memory_tiles_and_rows() {
        let m = MemoryStats {
            total: 32 << 30,
            used: 24 << 30,
            available: 7 << 30,
            modified: Some(1 << 30),
            cached: Some(6 << 30),
            committed: 30 << 30,
            commit_limit: 40 << 30,
            paged_pool: Some(1 << 30),
            non_paged_pool: Some(512 << 20),
        };
        let t = memory(&m);
        assert_eq!(t.value, "24.00 GB");
        assert_eq!(t.caption.unwrap(), "of 32.00 GB · 75% used");
        let c = committed(&m);
        assert_eq!((c.title, c.value.as_str()), ("Committed", "30.00 GB"));
        assert_eq!(c.caption.unwrap(), "of 40.00 GB commit limit · 75%");
        let rows = memory_rows(&m);
        let labels: Vec<_> = rows.iter().map(|r| r.0).collect();
        assert_eq!(
            labels,
            [
                "In use",
                "Available",
                "Cached",
                "Paged pool",
                "Non-paged pool",
                "Committed",
                "Usable physical memory"
            ]
        );
        assert_eq!(rows[5].1, "30.00 GB of 40.00 GB");
        let bare = memory_rows(&MemoryStats {
            cached: None,
            paged_pool: None,
            non_paged_pool: None,
            ..m
        });
        assert_eq!(bare.len(), 4);
    }

    #[cfg(windows)]
    #[test]
    fn windows_disk_uses_explorer_units() {
        let t = disk(Some(&StartupVolume {
            space: DiskSpace {
                total: 1_000_202_039_296,
                free: 511_101_108_224,
            },
            includes_purgeable: false,
        }));
        assert_eq!(t.title, "System drive");
        assert_eq!(t.value, "476.00 GB");
        let caption = t.caption.unwrap();
        assert!(caption.starts_with("free of 931.51 GB · "), "{caption}");
        assert!(caption.ends_with(':'), "{caption}");
        assert_eq!(disk(None).value, "Unknown");
    }

    #[cfg(windows)]
    #[test]
    fn windows_tiles_hide_what_windows_lacks() {
        let snap = Snapshot {
            ticks: Default::default(),
            cpu: None,
            memory: MemoryStats::default(),
            pressure: None,
            swap: None,
            load: None,
            boot_time: None,
            uptime_secs: Some(90),
            disk: None,
            cores: 8,
            physical_cores: 4,
            core_split: None,
            gpus: Vec::new(),
        };
        let titles: Vec<_> = tiles(&snap).iter().map(|t| t.title).collect();
        assert_eq!(
            titles,
            ["CPU", "Memory", "System drive", "Committed", "Uptime"]
        );
        let with_gpu = Snapshot {
            gpus: vec![GpuStats {
                name: "NVIDIA Quadro P1000".into(),
                utilization: 0.05,
                ..Default::default()
            }],
            ..snap
        };
        assert_eq!(tiles(&with_gpu)[1].title, "GPU");
    }
}
