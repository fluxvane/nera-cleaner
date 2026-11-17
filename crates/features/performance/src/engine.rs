//! Performance engine: live system statistics (`stats`), maintenance tasks
//! (`maintenance`) and the persisted last-run log (`log`). No UI; everything
//! here is synchronous and runs on a background executor.
//!
//! Port of `PerformanceViewModel.swift` (CPU ticks, `host_statistics64`
//! memory, Free RAM, Flush DNS), extended with swap, uptime, load averages and
//! the remaining maintenance tasks.
//!
//! `stats`, `gpu` and `maintenance` hold the shared types and logic; the OS
//! readers live in `*_macos.rs` / `*_windows.rs` siblings exporting the same
//! names, re-exported by the shared module.

pub mod disk;
pub mod gpu;
pub mod log;
pub mod maintenance;
pub mod present;
pub mod stats;
pub mod time;

#[cfg(target_os = "macos")]
mod gpu_macos;
#[cfg(windows)]
mod gpu_windows;
#[cfg(target_os = "macos")]
mod maintenance_macos;
#[cfg(windows)]
mod maintenance_windows;
#[cfg(windows)]
mod pdh;
#[cfg(target_os = "macos")]
mod stats_macos;
#[cfg(windows)]
mod stats_windows;

pub use gpu::GpuStats;
pub use log::{LogEntry, MaintenanceLog};
pub use maintenance::{RunError, TaskId};
pub use stats::{CpuLoad, CpuTicks, MemoryStats, Pressure, Snapshot, SwapUsage};
