//! privacy feature: core logic (`engine`) + screen(s). See docs/ARCHITECTURE.md.

/// File-system / OS logic for this feature. No UI; unit-tested.
pub mod engine;
/// Every string the screen shows, as pure functions (also used by `examples/dump.rs`).
pub mod view;

mod privacy_screen;
pub use privacy_screen::PrivacyScreen;
