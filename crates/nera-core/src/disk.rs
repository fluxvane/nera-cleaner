//! Volume capacity of the startup disk (the system drive on Windows).

use serde::Serialize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct DiskSpace {
    pub total: u64,
    pub free: u64,
}

impl DiskSpace {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }

    /// Used fraction, 0.0…1.0.
    pub fn used_fraction(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.used() as f64 / self.total as f64
        }
    }

    pub fn status(&self) -> DiskStatus {
        match self.used_fraction() {
            f if f >= 0.90 => DiskStatus::AlmostFull,
            f if f >= 0.75 => DiskStatus::Filling,
            _ => DiskStatus::Healthy,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskStatus {
    Healthy,
    Filling,
    AlmostFull,
}

impl DiskStatus {
    pub fn label(self) -> &'static str {
        match self {
            DiskStatus::Healthy => "Healthy",
            DiskStatus::Filling => "Filling up",
            DiskStatus::AlmostFull => "Almost full",
        }
    }
}

/// Raw capacity of the startup disk: the data volume's `statfs` figures on
/// macOS (what `df` and `diskutil` call free, without purgeable space), the
/// system drive's free and total bytes on Windows.
pub fn startup_disk() -> anyhow::Result<DiskSpace> {
    crate::sys::startup_disk()
}

/// Startup-disk capacity as Finder and System Settings report it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Volume {
    /// `space.free` is Finder's "Available" (free plus purgeable space macOS
    /// reclaims on demand) when `purgeable` is `Some`, else plain free space.
    pub space: DiskSpace,
    pub purgeable: Option<u64>,
}

/// Combine the raw capacity with Finder's "available for important usage" figure,
/// trusting the latter only when it is plausible (free <= it <= total).
pub fn resolve_volume(raw: DiskSpace, important: Option<u64>) -> Volume {
    match important {
        Some(avail) if raw.total > 0 && avail >= raw.free && avail <= raw.total => Volume {
            space: DiskSpace {
                total: raw.total,
                free: avail,
            },
            purgeable: Some(avail - raw.free),
        },
        _ => Volume {
            space: raw,
            purgeable: None,
        },
    }
}

/// Startup disk as Finder shows it ("X available of Y"); falls back to the
/// raw figures (with `purgeable: None`) when the system gives no such figure,
/// which is always the case on Windows.
pub fn startup_volume() -> anyhow::Result<Volume> {
    Ok(resolve_volume(
        startup_disk()?,
        crate::sys::important_usage(),
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_startup_disk() {
        let d = super::startup_disk().unwrap();
        assert!(d.total > 0 && d.free <= d.total);
    }

    #[test]
    fn finder_figure_is_used_only_when_plausible() {
        use super::{resolve_volume, DiskSpace};
        let raw = DiskSpace {
            total: 1_000,
            free: 100,
        };
        let v = resolve_volume(raw, Some(150));
        assert_eq!((v.space.free, v.purgeable), (150, Some(50)));
        for bad in [None, Some(50), Some(2_000)] {
            assert_eq!(
                resolve_volume(raw, bad),
                super::Volume {
                    space: raw,
                    purgeable: None
                }
            );
        }
    }

    #[test]
    fn live_volume_is_at_least_statfs_free() {
        let raw = super::startup_disk().unwrap();
        let v = super::startup_volume().unwrap();
        assert_eq!(v.space.total, raw.total);
        assert!(v.space.free <= v.space.total);
        // statfs is re-read, and other apps (backups, VMs) can free or use
        // gigabytes in between, so only a gross mismatch is a failure.
        assert!(v.space.free + 10_000_000_000 >= raw.free);
    }
}
