//! Thin, read-only wrappers over the macOS process APIs the engine needs
//! beyond what `sysinfo` offers. Every call is unprivileged and safe to make
//! for any pid; failures mean "macOS doesn't share this", never a made-up 0.

use std::ffi::c_void;

/// Facts every process reveals, even those owned by root or other users.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShortInfo {
    pub ppid: i32,
    /// Effective user id, the same one `ps -o user` reports.
    pub uid: u32,
    pub zombie: bool,
}

/// `PROC_PIDT_SHORTBSDINFO`: works for all processes (the full BSD info
/// does not, it fails for root and other users' processes).
pub fn short_info(pid: i32) -> Option<ShortInfo> {
    // SAFETY: zeroed POD struct; proc_pidinfo writes at most `size` bytes.
    let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdshortinfo).cast::<c_void>(),
            size,
        )
    };
    (got == size).then_some(ShortInfo {
        ppid: info.pbsi_ppid as i32,
        uid: info.pbsi_uid,
        zombie: info.pbsi_status == libc::SZOMB,
    })
}

/// The kernel's process name (`p_name`, up to 32 bytes): what `top` and
/// Activity Monitor show for non-app processes. `None` if unreadable.
pub fn proc_name(pid: i32) -> Option<String> {
    let mut buf = [0u8; 64];
    // SAFETY: the buffer is valid for `buf.len()` bytes.
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast::<c_void>(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    let name = String::from_utf8_lossy(&buf[..n as usize])
        .trim()
        .to_string();
    (!name.is_empty()).then_some(name)
}

/// Account name for a user id via the system directory (`getpwuid_r`), the
/// same lookup `ps` uses. `None` for an id with no account.
pub fn user_name(uid: u32) -> Option<String> {
    let mut buf = vec![0 as libc::c_char; 4096];
    // SAFETY: zeroed POD struct; getpwuid_r fills it and points into `buf`, both alive for the call.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
    if rc != 0 || result.is_null() || pwd.pw_name.is_null() {
        return None;
    }
    // SAFETY: pw_name is a NUL-terminated string inside `buf`.
    let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) }
        .to_string_lossy()
        .into_owned();
    (!name.is_empty()).then_some(name)
}

/// Counters of one process from `proc_pid_rusage`, readable for the user's
/// own processes only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    /// `ri_phys_footprint`: Activity Monitor's "Memory" column.
    pub footprint: u64,
    /// User + system CPU time consumed so far, in nanoseconds.
    pub cpu_ns: u64,
}

pub fn usage(pid: i32) -> Option<Usage> {
    // SAFETY: zeroed POD struct; proc_pid_rusage writes at most one rusage_info_v2.
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V2,
            (&mut info as *mut libc::rusage_info_v2).cast(),
        )
    };
    if rc != 0 || info.ri_phys_footprint == 0 {
        return None;
    }
    let (numer, denom) = timebase();
    let ticks = info.ri_user_time.saturating_add(info.ri_system_time);
    Some(Usage {
        footprint: info.ri_phys_footprint,
        cpu_ns: ticks_to_ns(ticks, numer, denom),
    })
}

/// Mach absolute time ticks to nanoseconds (125/3 on Apple silicon, 1/1 on Intel).
pub fn ticks_to_ns(ticks: u64, numer: u32, denom: u32) -> u64 {
    if denom == 0 {
        return ticks;
    }
    (u128::from(ticks) * u128::from(numer) / u128::from(denom)) as u64
}

#[allow(deprecated)] // libc points to the mach2 crate; one call isn't worth a dependency.
fn timebase() -> (u32, u32) {
    let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
    // SAFETY: plain out-parameter call.
    let rc = unsafe { libc::mach_timebase_info(&mut tb) };
    if rc != 0 || tb.denom == 0 {
        (1, 1)
    } else {
        (tb.numer, tb.denom)
    }
}

/// The host port, fetched once: each `mach_host_self` call adds a send right
/// that is never released, and this runs on every sample.
fn host_port() -> libc::mach_port_t {
    static PORT: std::sync::OnceLock<libc::mach_port_t> = std::sync::OnceLock::new();
    // SAFETY: no arguments; returns this task's host name port.
    #[allow(deprecated)]
    *PORT.get_or_init(|| unsafe { libc::mach_host_self() })
}

/// System "Memory Used" as Activity Monitor defines it:
/// App Memory (anonymous pages minus purgeable) + Wired + Compressed.
pub fn memory_used() -> Option<u64> {
    // SAFETY: zeroed POD struct filled by host_statistics64 within `count` words.
    let mut stat: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    #[allow(deprecated)]
    let rc = unsafe {
        libc::host_statistics64(
            host_port(),
            libc::HOST_VM_INFO64,
            (&mut stat as *mut libc::vm_statistics64).cast(),
            &mut count,
        )
    };
    if rc != 0 {
        return None;
    }
    // SAFETY: sysconf has no preconditions.
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .ok()
        .filter(|p| *p > 0)?;
    Some(used_from_pages(
        u64::from(stat.internal_page_count),
        u64::from(stat.purgeable_count),
        u64::from(stat.wire_count),
        u64::from(stat.compressor_page_count),
        page,
    ))
}

/// Pure arithmetic of [`memory_used`], pinned by a test.
pub fn used_from_pages(
    internal: u64,
    purgeable: u64,
    wired: u64,
    compressed: u64,
    page: u64,
) -> u64 {
    (internal.saturating_sub(purgeable) + wired + compressed) * page
}

/// CPU use in % of one core over a window: the Activity Monitor convention
/// (a process running flat out on 3 cores reads 300 %).
pub fn cpu_percent(delta_cpu_ns: u64, wall_ns: u64) -> f32 {
    if wall_ns == 0 {
        return 0.0;
    }
    (delta_cpu_ns as f64 * 100.0 / wall_ns as f64) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_used_is_app_plus_wired_plus_compressed() {
        // 1000 anonymous pages of which 100 purgeable, 200 wired, 300 compressed, 16 KiB pages.
        assert_eq!(
            used_from_pages(1000, 100, 200, 300, 16384),
            (900 + 200 + 300) * 16384
        );
        // Never underflows when purgeable exceeds internal.
        assert_eq!(used_from_pages(5, 10, 0, 0, 4096), 0);
    }

    #[test]
    fn converts_mach_ticks_with_the_timebase() {
        assert_eq!(ticks_to_ns(3_000_000, 125, 3), 125_000_000);
        assert_eq!(ticks_to_ns(42, 1, 1), 42);
        assert_eq!(ticks_to_ns(42, 1, 0), 42);
    }

    #[test]
    fn cpu_percent_is_relative_to_one_core() {
        assert_eq!(cpu_percent(1_000_000_000, 1_000_000_000), 100.0);
        assert_eq!(cpu_percent(3_000_000_000, 2_000_000_000), 150.0);
        assert_eq!(cpu_percent(0, 2_000_000_000), 0.0);
        assert_eq!(cpu_percent(5, 0), 0.0);
    }

    #[test]
    fn own_process_is_fully_readable() {
        let pid = std::process::id() as i32;
        let info = short_info(pid).expect("short info");
        assert_eq!(info.uid, unsafe { libc::geteuid() });
        assert!(!info.zombie);
        assert!(usage(pid).expect("usage").footprint > 0);
        assert!(proc_name(pid).is_some());
        // launchd is visible without privileges but its counters are not.
        assert_eq!(short_info(1).map(|i| i.uid), Some(0));
        assert_eq!(user_name(0).as_deref(), Some("root"));
        assert!(user_name(unsafe { libc::geteuid() }).is_some());
        assert_eq!(user_name(4_000_000_000), None);
    }

    /// Independent check of the mach-tick conversion: our CPU time for this
    /// process must equal what `getrusage` (timevals) reports for it.
    #[test]
    fn cpu_time_matches_getrusage() {
        let pid = std::process::id() as i32;
        let mut x = 1u64;
        for i in 0..200_000_000u64 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(i);
        }
        std::hint::black_box(x);
        // SAFETY: zeroed POD struct filled by getrusage.
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) }, 0);
        let tv = |t: libc::timeval| t.tv_sec as u64 * 1_000_000_000 + t.tv_usec as u64 * 1000;
        let reference = tv(ru.ru_utime) + tv(ru.ru_stime);
        let ours = usage(pid).expect("usage").cpu_ns;
        let diff = ours.abs_diff(reference);
        assert!(
            reference > 20_000_000,
            "the loop used some CPU: {reference}"
        );
        assert!(
            diff <= reference / 10 + 20_000_000,
            "ours {ours} ns vs getrusage {reference} ns"
        );
    }
}
