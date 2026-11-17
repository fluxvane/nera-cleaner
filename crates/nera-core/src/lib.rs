//! Nera Cleaner core: everything that touches the file system or the OS.
//! OS-specific code is confined to the private `sys` module.
//!
//! No UI code lives here. Every long-running function is synchronous and is
//! called by the app on a background executor; progress is reported through
//! plain callbacks. Every delete/trash path MUST go through [`safety`].
//! Feature logic lives in the feature crates under `crates/features/`.

// Shared foundation (owned by the app shell).
pub mod disk;
pub mod format;
pub mod fs_util;
pub mod os;
pub mod paths;
pub mod safety;
pub mod settings;
mod sys;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
