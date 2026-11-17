//! processes feature: core logic (`engine`) + screen(s). See docs/ARCHITECTURE.md.

/// File-system / OS logic for this feature. No UI; unit-tested.
pub mod engine;
/// Launch Services' registry of running apps (macOS's `lsappinfo`).
#[cfg(target_os = "macos")]
mod registry;
/// Process APIs: libproc and mach on macOS (`sys.rs`), the native API,
/// tokens and windows on Windows (`sys_windows.rs`).
#[cfg(target_os = "macos")]
mod sys;
#[cfg(windows)]
#[path = "sys_windows.rs"]
mod sys;

mod processes_screen;
pub use processes_screen::ProcessesScreen;
