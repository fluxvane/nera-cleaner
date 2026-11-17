//! Startup volume capacity as Finder and System Settings report it.
//!
//! `statfs` (and `df`) report only space that is free *right now*. Finder's
//! "Available" also counts purgeable space macOS reclaims on demand. The
//! reading lives in `nera_core::disk::startup_volume` so every screen shows
//! the same figure; this module only adapts it for the Performance screen.

use anyhow::Result;
use nera_core::disk::DiskSpace;

/// Capacity of the startup volume.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StartupVolume {
    pub space: DiskSpace,
    /// `space.free` includes purgeable space (Finder's "Available"). When
    /// `false` it is plain free space from `statfs`.
    pub includes_purgeable: bool,
}

/// Read the startup volume. Blocking but cheap.
pub fn read_startup_volume() -> Result<StartupVolume> {
    let v = nera_core::disk::startup_volume()?;
    Ok(StartupVolume {
        space: v.space,
        includes_purgeable: v.purgeable.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_volume_is_consistent_with_statfs() {
        let v = read_startup_volume().unwrap();
        let raw = nera_core::disk::startup_disk().unwrap();
        assert_eq!(v.space.total, raw.total);
        assert!(v.space.free <= v.space.total);
        assert!(
            v.space.free + 1_000_000_000 >= raw.free,
            "free space can only grow by purgeable space"
        );
    }
}
