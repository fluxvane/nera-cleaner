//! GPU load. The shared `GpuStats` type lives here; `read_gpus` comes from
//! `gpu_macos.rs` (IOKit `IOAccelerator` `PerformanceStatistics`, the source
//! Activity Monitor's GPU History uses) or `gpu_windows.rs` (the "GPU Engine"
//! performance counters Task Manager uses) and is re-exported here.

use std::collections::HashMap;

#[cfg(target_os = "macos")]
pub use super::gpu_macos::*;
#[cfg(windows)]
pub use super::gpu_windows::*;

/// Load of one GPU. Fractions are 0.0…1.0.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GpuStats {
    /// Marketing name, e.g. "Apple M1 Max" or "AMD Radeon Pro 5500M".
    pub name: String,
    /// Overall busy fraction ("Device Utilization %"; on Windows the busiest
    /// engine, as Task Manager shows it).
    pub utilization: f64,
    /// Fragment/shading work ("Renderer Utilization %"), if reported.
    pub renderer: Option<f64>,
    /// Geometry work ("Tiler Utilization %"), Apple GPUs only.
    pub tiler: Option<f64>,
    /// Memory the GPU driver currently uses ("In use system memory" on
    /// unified memory, "vramUsedBytes" on discrete GPUs; on Windows dedicated
    /// GPU memory in use, or shared GPU memory when the GPU has no dedicated
    /// memory).
    pub memory_used: Option<u64>,
    /// Memory allocated to the GPU ("Alloc system memory") or, on a discrete
    /// GPU, its total video memory (see [`GpuStats::memory_is_vram`]).
    pub memory_allocated: Option<u64>,
    /// `memory_allocated` is the card's VRAM size (a capacity), not an
    /// allocation out of system memory.
    pub memory_is_vram: bool,
}

impl GpuStats {
    /// Build from a `PerformanceStatistics` dictionary flattened to integers.
    /// Returns `None` when the driver reports no utilization figure.
    pub fn from_statistics(name: impl Into<String>, stats: &HashMap<String, i64>) -> Option<Self> {
        let pct = |key: &str| stats.get(key).map(|v| (*v as f64 / 100.0).clamp(0.0, 1.0));
        let bytes = |keys: &[&str]| {
            keys.iter()
                .find_map(|k| stats.get(*k))
                .filter(|v| **v > 0)
                .map(|v| *v as u64)
        };
        let utilization = pct("Device Utilization %").or_else(|| pct("GPU Activity(%)"))?;
        let memory_is_vram = !stats.contains_key("Alloc system memory")
            && stats.contains_key("vramFreeBytes")
            && stats.contains_key("vramUsedBytes");
        Some(Self {
            name: name.into(),
            utilization,
            renderer: pct("Renderer Utilization %"),
            tiler: pct("Tiler Utilization %"),
            memory_used: bytes(&["In use system memory", "vramUsedBytes"]),
            // Discrete GPUs report used and free VRAM; their sum is the VRAM size.
            memory_allocated: if memory_is_vram {
                let total = stats["vramFreeBytes"] + stats["vramUsedBytes"];
                (total > 0).then_some(total as u64)
            } else {
                bytes(&["Alloc system memory"])
            },
            memory_is_vram,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{read_gpus, GpuStats};
    use std::collections::HashMap;

    fn stats(pairs: &[(&str, i64)]) -> HashMap<String, i64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn maps_apple_silicon_statistics() {
        let s = stats(&[
            ("Device Utilization %", 18),
            ("Renderer Utilization %", 17),
            ("Tiler Utilization %", 12),
            ("In use system memory", 416_022_528),
            ("Alloc system memory", 4_385_554_432),
        ]);
        let gpu = GpuStats::from_statistics("Apple M1 Max", &s).unwrap();
        assert_eq!(gpu.name, "Apple M1 Max");
        assert!((gpu.utilization - 0.18).abs() < 1e-9);
        assert_eq!(gpu.renderer, Some(0.17));
        assert_eq!(gpu.tiler, Some(0.12));
        assert_eq!(gpu.memory_used, Some(416_022_528));
        assert_eq!(gpu.memory_allocated, Some(4_385_554_432));
        assert!(!gpu.memory_is_vram);
    }

    #[test]
    fn maps_discrete_gpu_vram() {
        let s = stats(&[
            ("Device Utilization %", 140),
            ("vramUsedBytes", 1_000),
            ("vramFreeBytes", 3_000),
        ]);
        let gpu = GpuStats::from_statistics("AMD Radeon", &s).unwrap();
        assert_eq!(gpu.utilization, 1.0, "clamped");
        assert_eq!(gpu.memory_used, Some(1_000));
        assert_eq!(gpu.memory_allocated, Some(4_000));
        assert!(gpu.memory_is_vram);
        assert_eq!(gpu.tiler, None);
    }

    #[test]
    fn no_utilization_means_no_gpu_entry() {
        assert!(GpuStats::from_statistics("x", &stats(&[("In use system memory", 5)])).is_none());
    }

    #[test]
    fn reads_live_gpus_without_crashing() {
        for gpu in read_gpus() {
            assert!(!gpu.name.is_empty());
            assert!((0.0..=1.0).contains(&gpu.utilization));
        }
    }
}
