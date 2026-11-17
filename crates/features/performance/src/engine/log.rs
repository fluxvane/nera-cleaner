//! When each maintenance task last ran, persisted to
//! `~/Library/Application Support/Nera Cleaner/maintenance.json`.

use super::maintenance::TaskId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// Unix seconds of the last successful run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<i64>,
    /// Error of the most recent attempt, cleared by a successful run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Task key (see [`TaskId::key`]) → entry. Unknown keys are kept so newer
/// versions' data survives a round trip.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintenanceLog {
    #[serde(default)]
    pub tasks: BTreeMap<String, LogEntry>,
}

impl MaintenanceLog {
    pub fn default_path() -> PathBuf {
        nera_core::paths::app_support_dir().join("maintenance.json")
    }

    /// Load from `path`; a missing or unreadable file yields an empty log.
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                ::log::warn!("ignoring unreadable {}: {err}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn load() -> Self {
        Self::load_from(&Self::default_path())
    }

    /// Write atomically (temp file + rename).
    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&Self::default_path())
    }

    pub fn entry(&self, id: TaskId) -> Option<&LogEntry> {
        self.tasks.get(id.key())
    }

    pub fn record_success(&mut self, id: TaskId, at: i64) {
        let e = self.tasks.entry(id.key().to_string()).or_default();
        e.last_run = Some(at);
        e.last_error = None;
    }

    pub fn record_failure(&mut self, id: TaskId, error: impl Into<String>) {
        self.tasks
            .entry(id.key().to_string())
            .or_default()
            .last_error = Some(error.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nera-perf-log-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn round_trips_through_json() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("nested/maintenance.json");
        let mut log = MaintenanceLog::default();
        log.record_success(TaskId::FlushDns, 1_700_000_000);
        log.record_failure(TaskId::FreeRam, "purge failed");
        log.save_to(&path).unwrap();
        let back = MaintenanceLog::load_from(&path);
        assert_eq!(back, log);
        assert_eq!(
            back.entry(TaskId::FlushDns).unwrap().last_run,
            Some(1_700_000_000)
        );
        assert_eq!(
            back.entry(TaskId::FreeRam).unwrap().last_error.as_deref(),
            Some("purge failed")
        );
        assert!(back.entry(TaskId::ResetQuickLook).is_none());
        assert!(!path.with_extension("json.tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn success_clears_previous_error() {
        let mut log = MaintenanceLog::default();
        log.record_failure(TaskId::RebuildSpotlight, "nope");
        log.record_success(TaskId::RebuildSpotlight, 5);
        let e = log.entry(TaskId::RebuildSpotlight).unwrap();
        assert_eq!(
            e,
            &LogEntry {
                last_run: Some(5),
                last_error: None
            }
        );
        // A later failure keeps the last successful run time.
        log.record_failure(TaskId::RebuildSpotlight, "again");
        assert_eq!(
            log.entry(TaskId::RebuildSpotlight).unwrap().last_run,
            Some(5)
        );
    }

    #[test]
    fn missing_or_corrupt_files_give_an_empty_log() {
        let dir = temp_dir("corrupt");
        assert_eq!(
            MaintenanceLog::load_from(&dir.join("absent.json")),
            MaintenanceLog::default()
        );
        let bad = dir.join("bad.json");
        std::fs::write(&bad, b"{not json").unwrap();
        assert_eq!(MaintenanceLog::load_from(&bad), MaintenanceLog::default());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn keeps_unknown_task_keys() {
        let json = r#"{"tasks":{"future-task":{"last_run":1},"flush-dns":{"last_run":2}}}"#;
        let log: MaintenanceLog = serde_json::from_str(json).unwrap();
        assert_eq!(log.tasks.len(), 2);
        assert_eq!(log.entry(TaskId::FlushDns).unwrap().last_run, Some(2));
    }
}
