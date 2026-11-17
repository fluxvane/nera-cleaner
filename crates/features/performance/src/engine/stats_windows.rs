//! Windows statistics, defined as on Task Manager's Performance page: CPU
//! time from `GetSystemTimes`, physical memory and commit charge from
//! `GlobalMemoryStatusEx`, the memory lists (modified, standby) and kernel
//! pools from the "Memory" performance counters, uptime from
//! `GetTickCount64`. Windows has no load average or memory-pressure level;
//! those snapshot fields stay `None`. Re-exported by `stats`.

use super::pdh::{Counter, Query};
use super::stats::{cpu_load, CpuTicks, Snapshot};
use anyhow::{bail, Result};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// CPU
// ---------------------------------------------------------------------------

/// Split `GetSystemTimes` totals (100 ns units, summed over all logical
/// processors) into ticks. Kernel time includes idle time.
pub fn ticks_from_system_times(idle: u64, kernel: u64, user: u64) -> CpuTicks {
    CpuTicks {
        user,
        system: kernel.saturating_sub(idle),
        idle,
        nice: 0,
    }
}

/// Current cumulative CPU time.
pub fn read_cpu_ticks() -> Result<CpuTicks> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetSystemTimes;
    let (mut idle, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: three valid out-pointers.
    if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
        bail!("GetSystemTimes failed: {}", std::io::Error::last_os_error());
    }
    let ft = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
    Ok(ticks_from_system_times(ft(idle), ft(kernel), ft(user)))
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

/// Memory in bytes, with Task Manager's names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryStats {
    /// Physical memory usable by Windows (`ullTotalPhys`; installed RAM minus
    /// hardware-reserved memory).
    pub total: u64,
    /// "In use": total minus available minus modified (the composition bar's
    /// first segment; In use + Modified + Standby + Free = total). Falls back
    /// to total minus available when the modified list cannot be read.
    pub used: u64,
    /// "Available": standby plus free memory (`ullAvailPhys`).
    pub available: u64,
    /// Modified list: data waiting to be written out before reuse.
    pub modified: Option<u64>,
    /// "Cached": standby plus modified lists.
    pub cached: Option<u64>,
    /// "Committed": commit charge (`ullTotalPageFile - ullAvailPageFile`).
    pub committed: u64,
    /// Commit limit: physical memory plus page files (`ullTotalPageFile`).
    pub commit_limit: u64,
    /// "Paged pool".
    pub paged_pool: Option<u64>,
    /// "Non-paged pool".
    pub non_paged_pool: Option<u64>,
}

/// `GlobalMemoryStatusEx` figures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryStatus {
    pub total_phys: u64,
    pub avail_phys: u64,
    pub total_page_file: u64,
    pub avail_page_file: u64,
}

/// "Memory" performance counters (bytes); `None` when a counter is missing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryLists {
    pub modified: Option<u64>,
    /// Standby list (core + normal priority + reserve).
    pub standby: Option<u64>,
    pub paged_pool: Option<u64>,
    pub non_paged_pool: Option<u64>,
}

impl MemoryStats {
    pub fn from_parts(s: &MemoryStatus, l: &MemoryLists) -> Self {
        let total = s.total_phys;
        let available = s.avail_phys.min(total);
        let modified = l.modified;
        let used = (total - available).saturating_sub(modified.unwrap_or(0));
        Self {
            total,
            used,
            available,
            modified,
            cached: l.standby.zip(modified).map(|(s, m)| s + m),
            committed: s.total_page_file.saturating_sub(s.avail_page_file),
            commit_limit: s.total_page_file,
            paged_pool: l.paged_pool,
            non_paged_pool: l.non_paged_pool,
        }
    }

    /// Used ("In use") fraction of physical memory, 0.0…1.0.
    pub fn used_fraction(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            (self.used as f64 / self.total as f64).clamp(0.0, 1.0)
        }
    }

    /// Committed fraction of the commit limit, 0.0…1.0.
    pub fn commit_fraction(&self) -> f64 {
        if self.commit_limit == 0 {
            0.0
        } else {
            (self.committed as f64 / self.commit_limit as f64).clamp(0.0, 1.0)
        }
    }
}

pub fn read_memory_status() -> Result<MemoryStatus> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `dwLength` is set as the API requires.
    if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 || m.ullTotalPhys == 0 {
        bail!(
            "GlobalMemoryStatusEx failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(MemoryStatus {
        total_phys: m.ullTotalPhys,
        avail_phys: m.ullAvailPhys,
        total_page_file: m.ullTotalPageFile,
        avail_page_file: m.ullAvailPageFile,
    })
}

struct ListCounters {
    query: Query,
    modified: Option<Counter>,
    standby: [Option<Counter>; 3],
    paged_pool: Option<Counter>,
    non_paged_pool: Option<Counter>,
}

static LISTS: Mutex<Option<ListCounters>> = Mutex::new(None);

/// Read the memory lists and pools. Instantaneous counters, so one collect
/// is enough; the query is kept open between calls.
pub fn read_memory_lists() -> MemoryLists {
    let mut guard = LISTS.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Query::open().map(|query| ListCounters {
            modified: query.add(r"\Memory\Modified Page List Bytes"),
            standby: [
                query.add(r"\Memory\Standby Cache Core Bytes"),
                query.add(r"\Memory\Standby Cache Normal Priority Bytes"),
                query.add(r"\Memory\Standby Cache Reserve Bytes"),
            ],
            paged_pool: query.add(r"\Memory\Pool Paged Bytes"),
            non_paged_pool: query.add(r"\Memory\Pool Nonpaged Bytes"),
            query,
        });
    }
    let Some(c) = guard.as_ref() else {
        return MemoryLists::default();
    };
    if !c.query.collect() {
        return MemoryLists::default();
    }
    let read = |counter: Option<Counter>| {
        counter
            .and_then(|k| c.query.value(k))
            .filter(|v| *v >= 0.0)
            .map(|v| v as u64)
    };
    let standby = c.standby.iter().map(|k| read(*k)).sum::<Option<u64>>();
    MemoryLists {
        modified: read(c.modified),
        standby,
        paged_pool: read(c.paged_pool),
        non_paged_pool: read(c.non_paged_pool),
    }
}

/// Current memory usage.
pub fn read_memory() -> Result<MemoryStats> {
    Ok(MemoryStats::from_parts(
        &read_memory_status()?,
        &read_memory_lists(),
    ))
}

// ---------------------------------------------------------------------------
// Uptime, processors
// ---------------------------------------------------------------------------

/// Time since boot in seconds (`GetTickCount64`, which keeps counting through
/// sleep and hibernation, like Task Manager's "Up time").
pub fn read_uptime_secs() -> u64 {
    // SAFETY: no arguments.
    unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() / 1000 }
}

/// Active logical processors across all processor groups (Task Manager's
/// "Logical processors").
pub fn core_count() -> usize {
    use windows_sys::Win32::System::Threading::{GetActiveProcessorCount, ALL_PROCESSOR_GROUPS};
    // SAFETY: plain query.
    match unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) } {
        0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
        n => n as usize,
    }
}

/// Physical cores (Task Manager's "Cores"): one `RelationProcessorCore`
/// record per core from `GetLogicalProcessorInformationEx`.
pub fn physical_core_count() -> usize {
    use windows_sys::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationProcessorCore,
        SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };
    let mut len = 0u32;
    // SAFETY: size query with a null buffer (fails with ERROR_INSUFFICIENT_BUFFER).
    unsafe {
        GetLogicalProcessorInformationEx(RelationProcessorCore, std::ptr::null_mut(), &mut len)
    };
    if len == 0 {
        return core_count();
    }
    // u64 elements keep the records 8-byte aligned.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    let ptr = buf
        .as_mut_ptr()
        .cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>();
    // SAFETY: `buf` holds at least `len` bytes.
    if unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, ptr, &mut len) } == 0 {
        return core_count();
    }
    let bytes = buf.as_ptr().cast::<u8>();
    let (mut offset, mut cores) = (0usize, 0usize);
    while offset + 8 <= len as usize {
        // SAFETY: each record starts with Relationship (i32) and Size (u32)
        // and lies within the `len` bytes written.
        let size = unsafe { bytes.add(offset + 4).cast::<u32>().read_unaligned() } as usize;
        if size == 0 {
            break;
        }
        cores += 1;
        offset += size;
    }
    if cores == 0 {
        core_count()
    } else {
        cores
    }
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// Read a full snapshot. With no previous ticks it takes a short second CPU
/// (and GPU) sample so the first result already has a CPU figure.
pub fn sample(prev: Option<CpuTicks>) -> Result<Snapshot> {
    let (prev, ticks) = match prev {
        Some(p) => (p, read_cpu_ticks()?),
        None => {
            let first = read_cpu_ticks()?;
            super::gpu::prime();
            std::thread::sleep(Duration::from_millis(400));
            (first, read_cpu_ticks()?)
        }
    };
    let uptime = read_uptime_secs();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok(Snapshot {
        ticks,
        cpu: cpu_load(&prev, &ticks),
        memory: read_memory()?,
        pressure: None,
        swap: None,
        load: None,
        boot_time: Some(now - uptime as i64).filter(|b| *b > 0),
        uptime_secs: Some(uptime),
        disk: super::disk::read_startup_volume().ok(),
        cores: core_count(),
        physical_cores: physical_core_count(),
        core_split: None,
        gpus: super::gpu::read_gpus(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_time_includes_idle() {
        let t = ticks_from_system_times(700, 900, 300);
        assert_eq!(
            t,
            CpuTicks {
                user: 300,
                system: 200,
                idle: 700,
                nice: 0
            }
        );
        // Never negative, even with an inconsistent read.
        assert_eq!(ticks_from_system_times(10, 5, 0).system, 0);
    }

    #[test]
    fn memory_matches_task_manager_definitions() {
        const GB: u64 = 1 << 30;
        let status = MemoryStatus {
            total_phys: 32 * GB,
            avail_phys: 12 * GB,
            total_page_file: 40 * GB,
            avail_page_file: 15 * GB,
        };
        let lists = MemoryLists {
            modified: Some(GB / 2),
            standby: Some(10 * GB),
            paged_pool: Some(GB),
            non_paged_pool: Some(GB / 4),
        };
        let m = MemoryStats::from_parts(&status, &lists);
        assert_eq!(m.total, 32 * GB);
        assert_eq!(m.available, 12 * GB);
        // In use = total - available - modified.
        assert_eq!(m.used, 32 * GB - 12 * GB - GB / 2);
        assert_eq!(m.cached, Some(10 * GB + GB / 2));
        assert_eq!(m.committed, 25 * GB);
        assert_eq!(m.commit_limit, 40 * GB);
        assert!((m.commit_fraction() - 0.625).abs() < 1e-9);
        assert!((m.used_fraction() - 19.5 / 32.0).abs() < 1e-9);

        // Without the counters: total - available, no cached figure.
        let m = MemoryStats::from_parts(&status, &MemoryLists::default());
        assert_eq!(m.used, 20 * GB);
        assert_eq!(m.cached, None);
        assert_eq!(m.paged_pool, None);

        // Inconsistent reads never underflow.
        let odd = MemoryStatus {
            total_phys: GB,
            avail_phys: 2 * GB,
            total_page_file: GB,
            avail_page_file: 2 * GB,
        };
        let m = MemoryStats::from_parts(&odd, &lists);
        assert_eq!((m.used, m.committed), (0, 0));
        assert_eq!(MemoryStats::default().used_fraction(), 0.0);
        assert_eq!(MemoryStats::default().commit_fraction(), 0.0);
    }

    /// `cargo test -p nera-performance -- --ignored --nocapture` to eyeball
    /// values against Task Manager.
    #[test]
    #[ignore]
    fn print_live_snapshot() {
        println!("{:#?}", sample(None).unwrap());
    }

    // Read-only checks against the live system.
    #[test]
    fn reads_live_statistics() {
        let snap = sample(None).unwrap();
        let m = snap.memory;
        assert!(m.total > 0 && m.used <= m.total && m.available <= m.total);
        assert!(m.committed <= m.commit_limit && m.commit_limit >= m.total / 2);
        assert!(m.modified.is_some(), "Memory counters readable");
        assert!(snap.cores >= 1);
        assert!(snap.physical_cores >= 1 && snap.physical_cores <= snap.cores);
        assert!(snap.uptime_secs.is_some_and(|u| u > 0));
        assert!(snap.boot_time.is_some());
        assert!((snap.pressure, snap.swap, snap.load) == (None, None, None));
        let cpu = snap.cpu.expect("a 400 ms window is long enough");
        assert!((cpu.user + cpu.system + cpu.idle - 1.0).abs() < 1e-6);
        let next = sample(Some(snap.ticks)).unwrap();
        assert!(next.ticks.idle >= snap.ticks.idle);
    }
}
