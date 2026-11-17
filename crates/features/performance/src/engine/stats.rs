//! Live system statistics. This module holds the platform-neutral types and
//! the CPU-load arithmetic; the readers (`read_cpu_ticks`, `read_memory`,
//! `sample`, ...) and `MemoryStats` come from `stats_macos.rs` (mach
//! `host_statistics`, `sysctl`, `getloadavg`) or `stats_windows.rs`
//! (`GetSystemTimes`, `GlobalMemoryStatusEx`, performance counters) and are
//! re-exported here. All reads are cheap and side-effect free.

use super::disk::StartupVolume;

#[cfg(target_os = "macos")]
pub use super::stats_macos::*;
#[cfg(windows)]
pub use super::stats_windows::*;

// ---------------------------------------------------------------------------
// CPU
// ---------------------------------------------------------------------------

/// Cumulative CPU ticks across all cores since boot (macOS: 10 ms scheduler
/// ticks; Windows: 100 ns units from `GetSystemTimes`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuTicks {
    pub user: u64,
    pub system: u64,
    pub idle: u64,
    pub nice: u64,
}

/// CPU time split over a sampling window, as fractions (sum ≈ 1.0).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CpuLoad {
    /// User time, including `nice`.
    pub user: f64,
    pub system: f64,
    pub idle: f64,
}

impl CpuLoad {
    /// Busy fraction (user + system).
    pub fn busy(&self) -> f64 {
        (1.0 - self.idle).clamp(0.0, 1.0)
    }
}

/// Minimum ticks in a window for a meaningful ratio (back-to-back samples
/// produce noise).
#[cfg(target_os = "macos")]
const MIN_WINDOW_TICKS: u64 = 20;
/// Minimum CPU time in a window for a meaningful ratio: 0.2 s summed over all
/// processors in 100 ns units, the same as macOS's 20 ticks of 10 ms.
#[cfg(windows)]
const MIN_WINDOW_TICKS: u64 = 2_000_000;

/// Elapsed ticks of one counter. The kernel's tick counters are 32-bit
/// (`natural_t`) and wrap after ~50 days of uptime on a 10-core Mac, so the
/// difference is taken modulo 2^32.
#[cfg(target_os = "macos")]
fn tick_delta(now: u64, prev: u64) -> u64 {
    (now as u32).wrapping_sub(prev as u32) as u64
}

/// Elapsed time of one counter. `GetSystemTimes` returns 64-bit totals that
/// only grow.
#[cfg(windows)]
fn tick_delta(now: u64, prev: u64) -> u64 {
    now.saturating_sub(prev)
}

/// Load between two tick samples; `None` when the window is too short for a
/// meaningful ratio.
pub fn cpu_load(prev: &CpuTicks, now: &CpuTicks) -> Option<CpuLoad> {
    let user = tick_delta(now.user, prev.user) + tick_delta(now.nice, prev.nice);
    let system = tick_delta(now.system, prev.system);
    let idle = tick_delta(now.idle, prev.idle);
    let total = user + system + idle;
    if total < MIN_WINDOW_TICKS {
        return None;
    }
    let t = total as f64;
    Some(CpuLoad {
        user: user as f64 / t,
        system: system as f64 / t,
        idle: idle as f64 / t,
    })
}

// ---------------------------------------------------------------------------
// Memory pressure, swap
// ---------------------------------------------------------------------------

/// Kernel memory pressure (`kern.memorystatus_vm_pressure_level`). macOS only:
/// Windows has no equivalent, so its snapshots carry `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Pressure {
    #[default]
    Normal,
    Warning,
    Critical,
}

impl Pressure {
    /// 1 = normal, 2 = warning, 4 = critical (`NOTE_MEMORYSTATUS_PRESSURE_*`).
    pub fn from_level(level: i32) -> Self {
        match level {
            l if l >= 4 => Pressure::Critical,
            2 | 3 => Pressure::Warning,
            _ => Pressure::Normal,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Pressure::Normal => "Normal",
            Pressure::Warning => "Elevated",
            Pressure::Critical => "Critical",
        }
    }
}

/// macOS swap (`vm.swapusage`). Windows reports its page file as commit
/// charge instead (see `MemoryStats` there); its snapshots carry `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SwapUsage {
    pub total: u64,
    pub used: u64,
    pub encrypted: bool,
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// Everything the Performance screen shows, read in one go.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Ticks at the time of this sample; pass back into the next [`sample`].
    pub ticks: CpuTicks,
    /// `None` until two samples far enough apart exist.
    pub cpu: Option<CpuLoad>,
    pub memory: MemoryStats,
    /// macOS only.
    pub pressure: Option<Pressure>,
    /// macOS only.
    pub swap: Option<SwapUsage>,
    /// macOS only (Windows has no load average).
    pub load: Option<[f64; 3]>,
    pub boot_time: Option<i64>,
    pub uptime_secs: Option<u64>,
    pub disk: Option<StartupVolume>,
    /// Logical CPUs (`hw.ncpu`; Windows: active logical processors).
    pub cores: usize,
    pub physical_cores: usize,
    /// Apple silicon P/E split; always `None` on Windows.
    pub core_split: Option<(usize, usize)>,
    /// One entry per GPU (usually one); empty when the OS reports none.
    pub gpus: Vec<super::gpu::GpuStats>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn cpu_load_splits_the_window() {
        let a = CpuTicks {
            user: 100,
            system: 50,
            idle: 800,
            nice: 0,
        };
        let b = CpuTicks {
            user: 160,
            system: 70,
            idle: 900,
            nice: 20,
        };
        let load = cpu_load(&a, &b).unwrap();
        // window: user 60 + nice 20, system 20, idle 100 → 200 ticks
        assert!((load.user - 0.4).abs() < 1e-9);
        assert!((load.system - 0.1).abs() < 1e-9);
        assert!((load.idle - 0.5).abs() < 1e-9);
        assert!((load.busy() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn cpu_load_rejects_short_windows() {
        let a = CpuTicks {
            user: 100,
            system: 50,
            idle: 800,
            nice: 0,
        };
        let tiny = CpuTicks {
            user: 101,
            system: 50,
            idle: 801,
            nice: 0,
        };
        assert!(cpu_load(&a, &tiny).is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cpu_load_survives_32_bit_counter_wrap() {
        // The kernel's tick counters are u32; idle wrapped between samples.
        let a = CpuTicks {
            user: 1_000,
            system: 500,
            idle: u32::MAX as u64 - 49,
            nice: 0,
        };
        let b = CpuTicks {
            user: 1_030,
            system: 520,
            idle: 50,
            nice: 0,
        };
        let load = cpu_load(&a, &b).unwrap();
        // user 30, system 20, idle 100 (50 before the wrap + 50 after)
        assert!((load.user - 0.2).abs() < 1e-9, "{load:?}");
        assert!((load.system - 20.0 / 150.0).abs() < 1e-9);
        assert!((load.idle - 100.0 / 150.0).abs() < 1e-9);
    }

    #[cfg(windows)]
    #[test]
    fn cpu_load_splits_a_windows_window() {
        // 100 ns units: 2 s on 8 logical processors = 160_000_000.
        let a = CpuTicks {
            user: 5_000_000_000,
            system: 1_000_000_000,
            idle: 90_000_000_000,
            nice: 0,
        };
        let b = CpuTicks {
            user: a.user + 40_000_000,
            system: a.system + 8_000_000,
            idle: a.idle + 112_000_000,
            nice: 0,
        };
        let load = cpu_load(&a, &b).unwrap();
        assert!((load.user - 0.25).abs() < 1e-9, "{load:?}");
        assert!((load.system - 0.05).abs() < 1e-9);
        assert!((load.idle - 0.7).abs() < 1e-9);
        assert!((load.busy() - 0.3).abs() < 1e-9);
        // 64-bit counters never wrap: a counter going backwards counts as 0.
        let back = CpuTicks {
            idle: a.idle - 1,
            ..b
        };
        assert_eq!(cpu_load(&a, &back).unwrap().idle, 0.0);
    }

    #[test]
    fn pressure_levels() {
        assert_eq!(Pressure::from_level(1), Pressure::Normal);
        assert_eq!(Pressure::from_level(0), Pressure::Normal);
        assert_eq!(Pressure::from_level(2), Pressure::Warning);
        assert_eq!(Pressure::from_level(4), Pressure::Critical);
    }
}
