//! devscan feature: core logic (`engine`) + screen(s). See docs/ARCHITECTURE.md.

/// File-system / OS logic for this feature. No UI; unit-tested.
pub mod engine;
/// Pure text shown by the screen.
pub mod view;

mod devscan_screen;
pub use devscan_screen::DevScanScreen;
