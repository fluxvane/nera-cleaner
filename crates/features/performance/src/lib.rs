//! performance feature: core logic (`engine`) + screen(s). See docs/ARCHITECTURE.md.

/// File-system / OS logic for this feature. No UI; unit-tested.
pub mod engine;

mod performance_screen;
pub use performance_screen::PerformanceScreen;
