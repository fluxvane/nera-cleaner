//! space feature: core logic (`engine`) + screen(s). See docs/ARCHITECTURE.md.

/// File-system / OS logic for this feature. No UI; unit-tested.
pub mod engine;

/// Pure text shown by the screen (unit-tested, printed by `examples/dump.rs`).
pub mod view;

mod space_screen;
pub use space_screen::SpaceScreen;
