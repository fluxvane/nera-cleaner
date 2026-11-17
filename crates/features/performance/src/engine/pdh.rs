//! Minimal wrapper over the Performance Data Helper (PDH) API: the
//! performance counters Task Manager, Resource Monitor and `Get-Counter`
//! read. Counter paths use English names (`PdhAddEnglishCounterW`), so they
//! work on every display language.

use windows_sys::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhGetFormattedCounterValue, PdhOpenQueryW, PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA,
    PDH_FMT_COUNTERVALUE, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
    PDH_MORE_DATA,
};

/// `PDH_FMT_NOCAP100` (missing from `windows-sys`): do not clamp percentages
/// to 100; the caller sums per-process values and clamps itself.
const PDH_FMT_NOCAP100: u32 = 0x0000_8000;
const FORMAT: u32 = PDH_FMT_DOUBLE | PDH_FMT_NOCAP100;

/// An open PDH query. Rate counters need two [`Query::collect`] calls before
/// they have a value; each later value covers the time since the previous
/// collect.
pub struct Query {
    handle: PDH_HQUERY,
}

// SAFETY: a PDH query handle is not tied to the thread that opened it; every
// user keeps its `Query` behind a `Mutex`, so calls never overlap.
unsafe impl Send for Query {}

/// A counter added to a [`Query`]; valid while that query is open.
#[derive(Clone, Copy, Debug)]
pub struct Counter(PDH_HCOUNTER);

// SAFETY: as for `Query`; the handle is only used with its owning query.
unsafe impl Send for Counter {}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn valid(status: u32) -> bool {
    status == PDH_CSTATUS_VALID_DATA || status == PDH_CSTATUS_NEW_DATA
}

impl Query {
    pub fn open() -> Option<Self> {
        let mut handle: PDH_HQUERY = std::ptr::null_mut();
        // SAFETY: null data source = live data; `handle` receives the query.
        let rc = unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut handle) };
        (rc == 0 && !handle.is_null()).then_some(Self { handle })
    }

    /// Add a counter by its English path, e.g. `\Memory\Available Bytes` or
    /// `\GPU Engine(*)\Utilization Percentage`. `None` when this PC does not
    /// have it.
    pub fn add(&self, path: &str) -> Option<Counter> {
        let path = wide(path);
        let mut counter: PDH_HCOUNTER = std::ptr::null_mut();
        // SAFETY: NUL-terminated path; `counter` receives the handle, owned by the query.
        let rc = unsafe { PdhAddEnglishCounterW(self.handle, path.as_ptr(), 0, &mut counter) };
        (rc == 0 && !counter.is_null()).then_some(Counter(counter))
    }

    /// Take a sample of every counter in the query.
    pub fn collect(&self) -> bool {
        // SAFETY: valid query handle.
        unsafe { PdhCollectQueryData(self.handle) == 0 }
    }

    /// Value of a single-instance counter at the last collect.
    pub fn value(&self, counter: Counter) -> Option<f64> {
        // SAFETY: all-zero is a valid PDH_FMT_COUNTERVALUE; the counter belongs to this query.
        let mut v: PDH_FMT_COUNTERVALUE = unsafe { std::mem::zeroed() };
        let rc =
            unsafe { PdhGetFormattedCounterValue(counter.0, FORMAT, std::ptr::null_mut(), &mut v) };
        if rc != 0 || !valid(v.CStatus) {
            return None;
        }
        // SAFETY: PDH_FMT_DOUBLE was requested, so the union holds a double.
        Some(unsafe { v.Anonymous.doubleValue })
    }

    /// `(instance name, value)` of every instance of a wildcard counter that
    /// has a valid value at the last collect. Instances that appeared since
    /// the previous collect have no rate yet and are left out.
    pub fn values(&self, counter: Counter) -> Vec<(String, f64)> {
        // Instances can appear between the size query and the read; retry.
        for _ in 0..4 {
            let mut size = 0u32;
            let mut count = 0u32;
            // SAFETY: size query with a null buffer.
            let rc = unsafe {
                PdhGetFormattedCounterArrayW(
                    counter.0,
                    FORMAT,
                    &mut size,
                    &mut count,
                    std::ptr::null_mut(),
                )
            };
            if rc != PDH_MORE_DATA || size == 0 {
                return Vec::new();
            }
            // `size` is in bytes and covers the items plus their name strings;
            // a buffer of whole items keeps the required alignment.
            let item = std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
            let mut buf =
                vec![PDH_FMT_COUNTERVALUE_ITEM_W::default(); (size as usize).div_ceil(item)];
            let mut size = (buf.len() * item) as u32;
            // SAFETY: `buf` holds `size` bytes; PDH writes `count` items followed
            // by the strings their `szName` pointers refer to.
            let rc = unsafe {
                PdhGetFormattedCounterArrayW(
                    counter.0,
                    FORMAT,
                    &mut size,
                    &mut count,
                    buf.as_mut_ptr(),
                )
            };
            if rc == PDH_MORE_DATA {
                continue;
            }
            if rc != 0 {
                return Vec::new();
            }
            return buf
                .iter()
                .take(count as usize)
                .filter(|it| valid(it.FmtValue.CStatus) && !it.szName.is_null())
                .map(|it| {
                    // SAFETY: `szName` points to a NUL-terminated string inside `buf`;
                    // the union holds a double (PDH_FMT_DOUBLE).
                    unsafe {
                        let len = (0..).take_while(|&i| *it.szName.add(i) != 0).count();
                        let name =
                            String::from_utf16_lossy(std::slice::from_raw_parts(it.szName, len));
                        (name, it.FmtValue.Anonymous.doubleValue)
                    }
                })
                .collect();
        }
        Vec::new()
    }
}

impl Drop for Query {
    fn drop(&mut self) {
        // SAFETY: the handle came from PdhOpenQueryW and is closed once; this
        // also frees its counters.
        unsafe {
            PdhCloseQuery(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_memory_and_processor_counters() {
        let q = Query::open().expect("PDH query");
        let avail = q.add(r"\Memory\Available Bytes").expect("counter");
        let cpu = q.add(r"\Processor(*)\% Processor Time").expect("counter");
        assert!(q.add(r"\No Such Object\Nothing").is_none());
        assert!(q.collect());
        assert!(q.value(avail).is_some_and(|v| v > 0.0));
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(q.collect());
        let per_cpu = q.values(cpu);
        assert!(
            per_cpu.iter().any(|(name, _)| name == "_Total"),
            "{per_cpu:?}"
        );
    }
}
