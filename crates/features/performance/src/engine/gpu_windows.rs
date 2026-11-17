//! GPU load on Windows, computed the way Task Manager's Performance page does
//! it: the "GPU Engine" performance counters report utilization per process
//! and engine (`pid_123_luid_0x0_0x1A2B_phys_0_eng_0_engtype_3D`); summing the
//! processes gives each engine's load, and a GPU's figure is its busiest
//! engine. Memory in use comes from the "GPU Adapter Memory" counters, names
//! and memory sizes from the display kernel (`D3DKMTQueryAdapterInfo`).
//! Software adapters (Microsoft Basic Render Driver) are skipped as Task
//! Manager does. Re-exported by `gpu`.

use super::gpu::GpuStats;
use super::pdh::{Counter, Query};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// Adapter LUID as one number (`HighPart << 32 | LowPart`), the key that ties
/// counter instances to adapters.
pub type Luid = u64;

/// LUID and engine index of a "GPU Engine" instance name.
pub fn parse_engine_instance(name: &str) -> Option<(Luid, u32)> {
    let luid = parse_luid(name)?;
    let eng = token_after(name, "eng")?.parse().ok()?;
    Some((luid, eng))
}

/// LUID of a "GPU Adapter Memory" instance name (`luid_0x0_0x1A2B_phys_0`).
pub fn parse_luid(name: &str) -> Option<Luid> {
    let mut tokens = name.split('_').skip_while(|t| *t != "luid").skip(1);
    let high = u32::from_str_radix(tokens.next()?.strip_prefix("0x")?, 16).ok()?;
    let low = u32::from_str_radix(tokens.next()?.strip_prefix("0x")?, 16).ok()?;
    Some(((high as u64) << 32) | low as u64)
}

fn token_after<'a>(name: &'a str, key: &str) -> Option<&'a str> {
    name.split('_').skip_while(|t| *t != key).nth(1)
}

/// Busiest engine per adapter, in percent (0…100): each engine's load is the
/// sum of its per-process instances, as in Task Manager.
pub fn utilization_by_adapter(samples: &[(String, f64)]) -> HashMap<Luid, f64> {
    let mut engines: HashMap<(Luid, u32), f64> = HashMap::new();
    for (name, value) in samples {
        if let Some(key) = parse_engine_instance(name) {
            *engines.entry(key).or_default() += value.max(0.0);
        }
    }
    let mut out: HashMap<Luid, f64> = HashMap::new();
    for ((luid, _), load) in engines {
        let busiest = out.entry(luid).or_default();
        *busiest = busiest.max(load.min(100.0));
    }
    out
}

/// Bytes per adapter from a "GPU Adapter Memory" counter.
pub fn bytes_by_adapter(samples: &[(String, f64)]) -> HashMap<Luid, u64> {
    let mut out: HashMap<Luid, u64> = HashMap::new();
    for (name, value) in samples {
        if let Some(luid) = parse_luid(name) {
            *out.entry(luid).or_default() += value.max(0.0) as u64;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Adapters
// ---------------------------------------------------------------------------

/// A display adapter as the kernel reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Adapter {
    pub luid: Luid,
    /// e.g. "NVIDIA Quadro P1000", as Task Manager and Device Manager show it.
    pub name: String,
    /// `D3DKMT_ADAPTERTYPE` bits.
    pub type_bits: u32,
    /// Dedicated video memory (VRAM); 0 on most integrated GPUs.
    pub dedicated_bytes: u64,
    /// System memory the GPU may share.
    pub shared_bytes: u64,
}

impl Adapter {
    const RENDER_SUPPORTED: u32 = 1 << 0;
    const SOFTWARE_DEVICE: u32 = 1 << 2;

    /// A hardware GPU that renders (not Basic Render, not display-only).
    pub fn is_gpu(&self) -> bool {
        self.type_bits & Self::RENDER_SUPPORTED != 0 && self.type_bits & Self::SOFTWARE_DEVICE == 0
    }
}

/// Every adapter the display kernel knows, in its enumeration order.
pub fn adapters() -> Vec<Adapter> {
    use windows_sys::Wdk::Graphics::Direct3D::{
        D3DKMTCloseAdapter, D3DKMTEnumAdapters2, D3DKMT_ADAPTERINFO, D3DKMT_ADAPTERREGISTRYINFO,
        D3DKMT_ADAPTERTYPE, D3DKMT_CLOSEADAPTER, D3DKMT_ENUMADAPTERS2, D3DKMT_SEGMENTSIZEINFO,
        KMTQAITYPE_ADAPTERREGISTRYINFO, KMTQAITYPE_ADAPTERTYPE, KMTQAITYPE_GETSEGMENTSIZE,
    };

    let mut list = Vec::new();
    // SAFETY: first call with a null buffer returns the adapter count; the
    // second fills `infos` (sized for that count) with opened adapter
    // handles, each closed below. Query outputs are POD structs whose size is
    // passed with them.
    unsafe {
        let mut e: D3DKMT_ENUMADAPTERS2 = std::mem::zeroed();
        if D3DKMTEnumAdapters2(&mut e) < 0 || e.NumAdapters == 0 {
            return list;
        }
        let mut infos: Vec<D3DKMT_ADAPTERINFO> = vec![std::mem::zeroed(); e.NumAdapters as usize];
        e.pAdapters = infos.as_mut_ptr();
        if D3DKMTEnumAdapters2(&mut e) < 0 {
            return list;
        }
        for info in infos.iter().take(e.NumAdapters as usize) {
            let h = info.hAdapter;
            let mut reg: D3DKMT_ADAPTERREGISTRYINFO = std::mem::zeroed();
            let mut ty: D3DKMT_ADAPTERTYPE = std::mem::zeroed();
            let mut seg: D3DKMT_SEGMENTSIZEINFO = std::mem::zeroed();
            let name = if query(h, KMTQAITYPE_ADAPTERREGISTRYINFO, &mut reg) {
                utf16_until_nul(&reg.AdapterString)
            } else {
                String::new()
            };
            let type_bits = if query(h, KMTQAITYPE_ADAPTERTYPE, &mut ty) {
                ty.Anonymous.Value
            } else {
                0
            };
            let have_seg = query(h, KMTQAITYPE_GETSEGMENTSIZE, &mut seg);
            let close = D3DKMT_CLOSEADAPTER { hAdapter: h };
            D3DKMTCloseAdapter(&close);
            let luid =
                ((info.AdapterLuid.HighPart as u32 as u64) << 32) | info.AdapterLuid.LowPart as u64;
            list.push(Adapter {
                luid,
                name,
                type_bits,
                dedicated_bytes: if have_seg {
                    seg.DedicatedVideoMemorySize
                } else {
                    0
                },
                shared_bytes: if have_seg {
                    seg.SharedSystemMemorySize
                } else {
                    0
                },
            });
        }
    }
    list
}

/// `D3DKMTQueryAdapterInfo` into `out`.
unsafe fn query<T>(adapter: u32, kind: i32, out: &mut T) -> bool {
    use windows_sys::Wdk::Graphics::Direct3D::{D3DKMTQueryAdapterInfo, D3DKMT_QUERYADAPTERINFO};
    let mut q = D3DKMT_QUERYADAPTERINFO {
        hAdapter: adapter,
        Type: kind,
        pPrivateDriverData: (out as *mut T).cast(),
        PrivateDriverDataSize: std::mem::size_of::<T>() as u32,
    };
    D3DKMTQueryAdapterInfo(&mut q) >= 0
}

fn utf16_until_nul(buf: &[u16]) -> String {
    let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len]).trim().to_string()
}

/// Combine an adapter with its counter readings. Discrete GPUs report their
/// dedicated memory in use and its size; GPUs without dedicated memory
/// report the shared memory in use.
pub fn gpu_stats(
    adapter: &Adapter,
    utilization_pct: Option<f64>,
    dedicated_used: Option<u64>,
    shared_used: Option<u64>,
) -> GpuStats {
    let vram = adapter.dedicated_bytes > 0;
    GpuStats {
        name: if adapter.name.is_empty() {
            "Graphics".into()
        } else {
            adapter.name.clone()
        },
        utilization: (utilization_pct.unwrap_or(0.0) / 100.0).clamp(0.0, 1.0),
        renderer: None,
        tiler: None,
        memory_used: if vram { dedicated_used } else { shared_used },
        memory_allocated: vram.then_some(adapter.dedicated_bytes),
        memory_is_vram: vram,
    }
}

// ---------------------------------------------------------------------------
// Sampling
// ---------------------------------------------------------------------------

struct Sampler {
    query: Query,
    engines: Counter,
    dedicated: Option<Counter>,
    shared: Option<Counter>,
}

impl Sampler {
    fn open() -> Option<Self> {
        let query = Query::open()?;
        let engines = query.add(r"\GPU Engine(*)\Utilization Percentage")?;
        let dedicated = query.add(r"\GPU Adapter Memory(*)\Dedicated Usage");
        let shared = query.add(r"\GPU Adapter Memory(*)\Shared Usage");
        Some(Self {
            query,
            engines,
            dedicated,
            shared,
        })
    }
}

enum State {
    Unopened,
    Open(Sampler),
    /// No "GPU Engine" counters on this PC (older Windows, some VMs).
    Unavailable,
}

static SAMPLER: Mutex<State> = Mutex::new(State::Unopened);

/// Open the counters if needed and take the first sample, so the next
/// [`read_gpus`] has a measurement window.
pub fn prime() {
    let mut state = SAMPLER.lock().unwrap_or_else(|e| e.into_inner());
    match &*state {
        State::Open(s) => {
            s.query.collect();
        }
        State::Unavailable => {}
        State::Unopened => {
            *state = match Sampler::open() {
                Some(s) => {
                    s.query.collect();
                    State::Open(s)
                }
                None => State::Unavailable,
            };
        }
    }
}

/// Every hardware GPU's load over the time since the previous call (or
/// [`prime`]). The first call ever waits half a second for a window. Empty
/// when Windows exposes no GPU counters.
pub fn read_gpus() -> Vec<GpuStats> {
    let mut state = SAMPLER.lock().unwrap_or_else(|e| e.into_inner());
    if matches!(*state, State::Unopened) {
        *state = match Sampler::open() {
            Some(s) => {
                s.query.collect();
                std::thread::sleep(Duration::from_millis(500));
                State::Open(s)
            }
            None => State::Unavailable,
        };
    }
    let State::Open(s) = &*state else {
        return Vec::new();
    };
    if !s.query.collect() {
        return Vec::new();
    }
    let util = utilization_by_adapter(&s.query.values(s.engines));
    let dedicated = s
        .dedicated
        .map(|c| bytes_by_adapter(&s.query.values(c)))
        .unwrap_or_default();
    let shared = s
        .shared
        .map(|c| bytes_by_adapter(&s.query.values(c)))
        .unwrap_or_default();
    drop(state);
    adapters()
        .iter()
        .filter(|a| a.is_gpu())
        .map(|a| {
            gpu_stats(
                a,
                util.get(&a.luid).copied(),
                dedicated.get(&a.luid).copied(),
                shared.get(&a.luid).copied(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(pairs: &[(&str, f64)]) -> Vec<(String, f64)> {
        pairs.iter().map(|(n, v)| (n.to_string(), *v)).collect()
    }

    #[test]
    fn parses_instance_names() {
        assert_eq!(
            parse_engine_instance(
                "pid_31964_luid_0x00000000_0x000116E4_phys_0_eng_3_engtype_VideoDecode"
            ),
            Some((0x116e4, 3))
        );
        assert_eq!(
            parse_engine_instance(
                "pid_4_luid_0x00000001_0x00012582_phys_0_eng_7_engtype_Compute_1"
            ),
            Some(((1u64 << 32) | 0x12582, 7))
        );
        assert_eq!(
            parse_luid("luid_0x00000000_0x00012582_phys_0"),
            Some(0x12582)
        );
        assert_eq!(parse_luid("_Total"), None);
        assert_eq!(parse_engine_instance("luid_0x0_0xZZ_phys_0_eng_0"), None);
    }

    #[test]
    fn gpu_load_is_the_busiest_engine_summed_over_processes() {
        let samples = s(&[
            (
                "pid_1_luid_0x00000000_0x000116e4_phys_0_eng_0_engtype_3D",
                12.5,
            ),
            (
                "pid_2_luid_0x00000000_0x000116e4_phys_0_eng_0_engtype_3D",
                20.0,
            ),
            (
                "pid_2_luid_0x00000000_0x000116e4_phys_0_eng_5_engtype_Copy",
                40.0,
            ),
            (
                "pid_3_luid_0x00000000_0x00012582_phys_0_eng_0_engtype_3D",
                70.0,
            ),
            (
                "pid_4_luid_0x00000000_0x00012582_phys_0_eng_0_engtype_3D",
                60.0,
            ),
            ("garbage", 99.0),
        ]);
        let util = utilization_by_adapter(&samples);
        assert_eq!(util.len(), 2);
        // 3D: 12.5 + 20 = 32.5, Copy: 40 → 40.
        assert!((util[&0x116e4] - 40.0).abs() < 1e-9);
        // 70 + 60 on one engine is capped at 100.
        assert_eq!(util[&0x12582], 100.0);
    }

    #[test]
    fn memory_and_names_per_adapter() {
        let bytes = bytes_by_adapter(&s(&[
            ("luid_0x00000000_0x00012582_phys_0", 691_806_208.0),
            ("luid_0x00000000_0x000116e4_phys_0", 0.0),
        ]));
        assert_eq!(bytes[&0x12582], 691_806_208);

        let discrete = Adapter {
            luid: 0x12582,
            name: "NVIDIA Quadro P1000".into(),
            type_bits: 0b011,
            dedicated_bytes: 4 << 30,
            shared_bytes: 16 << 30,
        };
        assert!(discrete.is_gpu());
        let g = gpu_stats(&discrete, Some(37.0), Some(691_806_208), Some(5));
        assert_eq!(g.name, "NVIDIA Quadro P1000");
        assert!((g.utilization - 0.37).abs() < 1e-9);
        assert_eq!(g.memory_used, Some(691_806_208));
        assert_eq!(g.memory_allocated, Some(4 << 30));
        assert!(g.memory_is_vram);
        assert_eq!((g.renderer, g.tiler), (None, None));

        let integrated = Adapter {
            dedicated_bytes: 0,
            name: String::new(),
            ..discrete.clone()
        };
        let g = gpu_stats(&integrated, None, Some(0), Some(300 << 20));
        assert_eq!(g.name, "Graphics");
        assert_eq!(g.utilization, 0.0);
        assert_eq!(g.memory_used, Some(300 << 20));
        assert_eq!(g.memory_allocated, None);
        assert!(!g.memory_is_vram);

        let basic_render = Adapter {
            type_bits: 0b101,
            ..discrete.clone()
        };
        assert!(!basic_render.is_gpu());
        let display_only = Adapter {
            type_bits: 0b010,
            ..discrete
        };
        assert!(!display_only.is_gpu());
    }

    #[test]
    fn enumerates_live_adapters() {
        let all = adapters();
        for a in &all {
            assert_ne!(a.luid, 0, "{a:?}");
        }
        // Read-only: two samples through the shared sampler.
        prime();
        std::thread::sleep(Duration::from_millis(300));
        let gpus = read_gpus();
        assert!(gpus.len() <= all.iter().filter(|a| a.is_gpu()).count());
    }
}
