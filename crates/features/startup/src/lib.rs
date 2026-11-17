//! startup feature: core logic (`engine`) + screen(s). See docs/ARCHITECTURE.md.
//!
//! macOS: launch agents/daemons and login items (`engine.rs`, `view.rs`,
//! `startup_screen.rs`). Windows: startup apps as Task Manager lists them
//! (`engine_windows.rs`, `view_windows.rs`, `startup_screen_windows.rs`).
//! Other platforms: empty.

/// File-system / OS logic for this feature. No UI; unit-tested.
#[cfg(target_os = "macos")]
pub mod engine;
/// File-system / OS logic for this feature. No UI; unit-tested.
#[cfg(windows)]
#[path = "engine_windows.rs"]
pub mod engine;

/// Every string the screen shows, as pure functions (also used by `examples/dump_startup.rs`).
#[cfg(target_os = "macos")]
pub mod view;
/// Every string the screen shows, as pure functions (also used by `examples/dump_startup.rs`).
#[cfg(windows)]
#[path = "view_windows.rs"]
pub mod view;

#[cfg(target_os = "macos")]
mod startup_screen;
#[cfg(windows)]
#[path = "startup_screen_windows.rs"]
mod startup_screen;

#[cfg(any(target_os = "macos", windows))]
pub use startup_screen::StartupScreen;
