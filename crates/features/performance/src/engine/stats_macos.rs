//! macOS statistics read straight from the kernel: CPU ticks
//! (`host_statistics`), memory (`host_statistics64`), memory pressure, swap,
//! boot time and load averages (`sysctl`, `getloadavg`). Re-exported by
//! `stats`.

use super::stats::{cpu_load, CpuTicks, Pressure, Snapshot, SwapUsage};
use anyhow::{bail, Result};
use std::ffi::CString;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// CPU
// ---------------------------------------------------------------------------

/// Current cumulative CPU ticks.
pub fn read_cpu_ticks() -> Result<CpuTicks> {
    // SAFETY: zeroed POD out-struct; count matches its size in integer_t units.
    let mut info: libc::host_cpu_load_info = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    let kr = unsafe {
        libc::host_statistics(
            host_port(),
            libc::HOST_CPU_LOAD_INFO,
            (&mut info as *mut libc::host_cpu_load_info).cast(),
            &mut count,
        )
    };
    if kr != libc::KERN_SUCCESS {
        bail!("host_statistics(HOST_CPU_LOAD_INFO) failed with code {kr}");
    }
    let t = info.cpu_ticks;
    Ok(CpuTicks {
        user: t[libc::CPU_STATE_USER as usize] as u64,
        system: t[libc::CPU_STATE_SYSTEM as usize] as u64,
        idle: t[libc::CPU_STATE_IDLE as usize] as u64,
        nice: t[libc::CPU_STATE_NICE as usize] as u64,
    })
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

/// Raw page counts from `host_statistics64(HOST_VM_INFO64)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VmCounts {
    pub page_size: u64,
    /// `free_count` as the kernel reports it: free pages *plus* speculative ones.
    pub free: u64,
    pub speculative: u64,
    pub wired: u64,
    pub purgeable: u64,
    /// Pages occupied by the compressor.
    pub compressor: u64,
    /// File-backed pages.
    pub external: u64,
    /// Anonymous pages.
    pub internal: u64,
}

/// Memory usage in bytes, split the way Activity Monitor does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryStats {
    pub total: u64,
    /// App + wired + compressed.
    pub used: u64,
    /// Anonymous memory owned by processes (minus purgeable).
    pub app: u64,
    pub wired: u64,
    pub compressed: u64,
    /// File cache and purgeable memory; reclaimed instantly when needed.
    pub cached: u64,
    /// Pages on the free list (`vm_stat`'s "Pages free", `top`'s "unused"). Not
    /// derived from the other rows: a few hundred MB of kernel and
    /// reserved memory is neither used, cached nor free.
    pub free: u64,
}

impl MemoryStats {
    pub fn from_counts(c: &VmCounts, total: u64) -> Self {
        let p = c.page_size;
        let app = c.internal.saturating_sub(c.purgeable) * p;
        let wired = c.wired * p;
        let compressed = c.compressor * p;
        let used = app + wired + compressed;
        let cached = (c.external + c.purgeable) * p;
        // `free_count` includes speculative pages, which `vm_stat` (and the
        // file-backed count above) already treat separately.
        let free = c.free.saturating_sub(c.speculative) * p;
        Self {
            total,
            used,
            app,
            wired,
            compressed,
            cached,
            free,
        }
    }

    /// Used fraction of physical memory, 0.0…1.0.
    pub fn used_fraction(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            (self.used as f64 / self.total as f64).clamp(0.0, 1.0)
        }
    }
}

pub fn read_vm_counts() -> Result<VmCounts> {
    // SAFETY: zeroed POD out-struct; count is the struct size in integer_t
    // units, and the kernel never writes more than `count` words.
    let mut vm: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    let kr = unsafe {
        libc::host_statistics64(
            host_port(),
            libc::HOST_VM_INFO64,
            (&mut vm as *mut libc::vm_statistics64).cast(),
            &mut count,
        )
    };
    if kr != libc::KERN_SUCCESS {
        bail!("host_statistics64(HOST_VM_INFO64) failed with code {kr}");
    }
    Ok(VmCounts {
        page_size: page_size(),
        free: vm.free_count as u64,
        speculative: vm.speculative_count as u64,
        wired: vm.wire_count as u64,
        purgeable: vm.purgeable_count as u64,
        compressor: vm.compressor_page_count as u64,
        external: vm.external_page_count as u64,
        internal: vm.internal_page_count as u64,
    })
}

/// Current memory usage.
pub fn read_memory() -> Result<MemoryStats> {
    let total = sysctl::<u64>("hw.memsize").unwrap_or(0);
    if total == 0 {
        bail!("could not read installed memory (hw.memsize)");
    }
    Ok(MemoryStats::from_counts(&read_vm_counts()?, total))
}

/// `None` when the kernel does not report a level (never shown as "Normal").
pub fn read_pressure() -> Option<Pressure> {
    sysctl::<i32>("kern.memorystatus_vm_pressure_level").map(Pressure::from_level)
}

fn page_size() -> u64 {
    sysctl::<i32>("vm.pagesize")
        .filter(|p| *p > 0)
        .map(|p| p as u64)
        // SAFETY: plain read of a libSystem global.
        .unwrap_or_else(|| unsafe { libc::vm_page_size } as u64)
}

// ---------------------------------------------------------------------------
// Swap, uptime, load
// ---------------------------------------------------------------------------

/// `None` when `vm.swapusage` cannot be read (never shown as "0 bytes").
pub fn read_swap() -> Option<SwapUsage> {
    sysctl::<libc::xsw_usage>("vm.swapusage").map(|s| SwapUsage {
        total: s.xsu_total,
        used: s.xsu_used,
        encrypted: s.xsu_encrypted != 0,
    })
}

/// Boot time as Unix seconds.
pub fn read_boot_time() -> Option<i64> {
    sysctl::<libc::timeval>("kern.boottime")
        .map(|tv| tv.tv_sec)
        .filter(|s| *s > 0)
}

/// 1, 5 and 15 minute load averages.
pub fn read_load_average() -> Option<[f64; 3]> {
    let mut avg = [0f64; 3];
    // SAFETY: buffer holds exactly 3 doubles.
    let n = unsafe { libc::getloadavg(avg.as_mut_ptr(), 3) };
    (n == 3).then_some(avg)
}

/// Logical CPU count.
pub fn core_count() -> usize {
    sysctl::<i32>("hw.ncpu")
        .filter(|n| *n > 0)
        .map(|n| n as usize)
        .or_else(|| std::thread::available_parallelism().ok().map(|n| n.get()))
        .unwrap_or(1)
}

/// Physical core count (`hw.physicalcpu`); equals [`core_count`] unless the
/// CPU has hyper-threading.
pub fn physical_core_count() -> usize {
    sysctl::<i32>("hw.physicalcpu")
        .filter(|n| *n > 0)
        .map(|n| n as usize)
        .unwrap_or_else(core_count)
}

/// Performance / efficiency core split on Apple silicon, if reported.
pub fn core_split() -> Option<(usize, usize)> {
    let p = sysctl::<i32>("hw.perflevel0.logicalcpu")?;
    let e = sysctl::<i32>("hw.perflevel1.logicalcpu")?;
    (p > 0 && e > 0).then_some((p as usize, e as usize))
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// Read a full snapshot. With no previous ticks it takes a short second CPU
/// sample so the first result already has a CPU figure.
pub fn sample(prev: Option<CpuTicks>) -> Result<Snapshot> {
    let (prev, ticks) = match prev {
        Some(p) => (p, read_cpu_ticks()?),
        None => {
            let first = read_cpu_ticks()?;
            std::thread::sleep(Duration::from_millis(400));
            (first, read_cpu_ticks()?)
        }
    };
    let boot_time = read_boot_time();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok(Snapshot {
        ticks,
        cpu: cpu_load(&prev, &ticks),
        memory: read_memory()?,
        pressure: read_pressure(),
        swap: read_swap(),
        load: read_load_average(),
        boot_time,
        uptime_secs: boot_time.filter(|b| *b <= now).map(|b| (now - b) as u64),
        disk: super::disk::read_startup_volume().ok(),
        cores: core_count(),
        physical_cores: physical_core_count(),
        core_split: core_split(),
        gpus: super::gpu::read_gpus(),
    })
}

// ---------------------------------------------------------------------------
// FFI helpers
// ---------------------------------------------------------------------------

extern "C" {
    // Not exported by `libc` without a deprecation warning, and absent from
    // the workspace's `mach2` version.
    fn mach_host_self() -> libc::mach_port_t;
}

/// The host port, fetched once (each `mach_host_self` call adds a send right).
fn host_port() -> libc::mach_port_t {
    static PORT: OnceLock<libc::mach_port_t> = OnceLock::new();
    // SAFETY: no arguments; returns this task's host name port.
    *PORT.get_or_init(|| unsafe { mach_host_self() })
}

/// Fixed-size `sysctlbyname` read. Only for plain-old-data `T` (integers,
/// `timeval`, `xsw_usage`) for which all-zero bytes are a valid value.
fn sysctl<T: Copy>(name: &str) -> Option<T> {
    let c = CString::new(name).ok()?;
    // SAFETY: T is POD (see doc); the kernel writes at most `len` bytes.
    let mut value: T = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<T>();
    let rc = unsafe {
        libc::sysctlbyname(
            c.as_ptr(),
            (&mut value as *mut T).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && len == std::mem::size_of::<T>()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_matches_activity_monitor_split() {
        let c = VmCounts {
            page_size: 16_384,
            free: 100,
            speculative: 10,
            wired: 200,
            purgeable: 50,
            compressor: 300,
            external: 400,
            internal: 1_050,
        };
        let total = 4_000 * 16_384;
        let m = MemoryStats::from_counts(&c, total);
        assert_eq!(m.app, 1_000 * 16_384);
        assert_eq!(m.wired, 200 * 16_384);
        assert_eq!(m.compressed, 300 * 16_384);
        assert_eq!(m.used, 1_500 * 16_384);
        assert_eq!(m.cached, 450 * 16_384);
        // Free = free_count - speculative = vm_stat "Pages free".
        assert_eq!(m.free, 90 * 16_384);
        assert!((m.used_fraction() - 0.375).abs() < 1e-9);
    }

    #[test]
    fn memory_free_never_underflows() {
        let c = VmCounts {
            page_size: 4096,
            wired: 10,
            internal: 10,
            external: 10,
            ..Default::default()
        };
        let m = MemoryStats::from_counts(&c, 4096);
        assert_eq!(m.free, 0);
        assert_eq!(m.used_fraction(), 1.0);
        // speculative > free_count must not wrap
        let c = VmCounts {
            page_size: 4096,
            free: 5,
            speculative: 9,
            ..Default::default()
        };
        assert_eq!(MemoryStats::from_counts(&c, 4096).free, 0);
        assert_eq!(MemoryStats::default().used_fraction(), 0.0);
    }

    /// `cargo test -p nera-performance -- --ignored --nocapture` to eyeball
    /// values against Activity Monitor.
    #[test]
    #[ignore]
    fn print_live_snapshot() {
        println!("{:#?}", sample(None).unwrap());
    }

    // Read-only checks against the live system.
    #[test]
    fn reads_live_statistics() {
        let snap = sample(None).unwrap();
        assert!(snap.memory.total > 0);
        assert!(snap.memory.used <= snap.memory.total + snap.memory.compressed);
        assert!(snap.cores >= 1);
        assert!(snap.uptime_secs.is_some());
        let swap = snap.swap.expect("vm.swapusage readable");
        assert!(swap.used <= swap.total || swap.total == 0);
        assert!(snap.pressure.is_some());
        assert!(snap.physical_cores >= 1 && snap.physical_cores <= snap.cores);
        let load = snap.load.unwrap();
        assert!(load.iter().all(|l| *l >= 0.0));
        if let Some(cpu) = snap.cpu {
            assert!((cpu.user + cpu.system + cpu.idle - 1.0).abs() < 1e-6);
        }
        let next = sample(Some(snap.ticks)).unwrap();
        assert!(next.ticks.idle >= snap.ticks.idle);
        assert!(page_size() >= 4096);
    }
}
