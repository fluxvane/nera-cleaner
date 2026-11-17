//! duplicates feature: core logic (`engine`) + screen(s). See docs/ARCHITECTURE.md.

/// File-system / OS logic for this feature. No UI; unit-tested.
pub mod engine;
/// Pure text shown by the screen.
pub mod view;

mod duplicates_screen;
pub use duplicates_screen::DuplicatesScreen;
