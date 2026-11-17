//! Platform layer: the only place in `nera-core` with OS-specific code.
//! `macos.rs` and `windows.rs` export the same functions; the public modules
//! (`paths`, `fs_util`, `disk`, `safety`) are thin wrappers over them.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(not(any(target_os = "macos", windows)))]
compile_error!("Nera Cleaner supports macOS and Windows only");

/// Identity of a file on disk: two paths with the same `volume` and `index`
/// are hard links to the same data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileKey {
    pub volume: u64,
    pub index: u64,
    /// Number of hard links to the file (1 for an ordinary file).
    pub links: u64,
}

impl FileKey {
    pub fn id(&self) -> (u64, u64) {
        (self.volume, self.index)
    }
}
