//! Battery and sensor engine (no UI). Port of BatteryService, TemperatureService
//! and FanService, with a real SMC reader instead of the Swift stubs.
//!
//! macOS: IOKit/SMC modules below. Windows: `battery_windows.rs` and
//! `present_windows.rs`, mounted as `battery` and `present` so the screen and
//! the dump example use the same paths on both platforms.

#[cfg(target_os = "macos")]
pub mod battery;
#[cfg(target_os = "macos")]
pub mod health;
#[cfg(target_os = "macos")]
pub mod sensors;
#[cfg(target_os = "macos")]
pub mod smc;

#[cfg(target_os = "macos")]
pub use battery::{read_battery, BatteryInfo, Condition, Health, Source};
#[cfg(target_os = "macos")]
pub use sensors::{read_sensors, Fan, Level, SensorKind, SensorSnapshot, Temperature};

#[cfg(target_os = "macos")]
pub mod present;

#[cfg(windows)]
#[path = "battery_windows.rs"]
pub mod battery;
#[cfg(windows)]
#[path = "present_windows.rs"]
pub mod present;

#[cfg(windows)]
pub use battery::{read_battery, BatteryInfo, PowerStatus, Unit};
