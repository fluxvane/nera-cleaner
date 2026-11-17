//! apps feature: core logic (`engine`) + screen(s). See docs/ARCHITECTURE.md.
//!
//! The two platforms share the screen's layout but not its model: macOS lists
//! `.app` bundles and moves them (plus leftovers) to the Trash; Windows lists
//! the programs Settings > Apps shows and opens each one's own uninstaller. So
//! each platform has its own `engine`, `view` and screen module under the same
//! names (`engine.rs`, `view.rs`, `apps_screen.rs` are the macOS ones).

/// File-system / OS logic for this feature. No UI; unit-tested.
#[cfg(target_os = "macos")]
pub mod engine;
#[cfg(windows)]
#[path = "engine_windows.rs"]
pub mod engine;

/// Pure text shown by the screen (unit-tested, printed by `examples/dump_apps.rs`).
#[cfg(target_os = "macos")]
pub mod view;
#[cfg(windows)]
#[path = "view_windows.rs"]
pub mod view;

#[cfg(target_os = "macos")]
mod apps_screen;
#[cfg(windows)]
#[path = "apps_screen_windows.rs"]
mod apps_screen;

#[cfg(any(target_os = "macos", windows))]
pub use apps_screen::AppsScreen;
