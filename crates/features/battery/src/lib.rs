//! battery feature: core logic (`engine`) + screen(s). See docs/ARCHITECTURE.md.
//!
//! macOS: battery, temperatures and fans. Windows: battery only (Windows has
//! no supported API for CPU/GPU temperatures or fan speeds). Other platforms
//! have no engine, so the crate is empty there.
#![cfg(any(target_os = "macos", target_os = "windows"))]

/// File-system / OS logic for this feature. No UI; unit-tested.
pub mod engine;

mod battery_screen;
pub use battery_screen::BatteryScreen;
