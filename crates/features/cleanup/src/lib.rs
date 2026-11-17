//! cleanup feature: core logic (`engine`) + screen(s). See docs/ARCHITECTURE.md.

/// File-system / OS logic for this feature. No UI; unit-tested.
pub mod engine;

mod dashboard_screen;
pub use dashboard_screen::DashboardScreen;
mod cleanup_screen;
pub use cleanup_screen::CleanupScreen;

pub mod present;
mod store;
pub use store::{scan_store, Phase, ScanResults, ScanStore};
