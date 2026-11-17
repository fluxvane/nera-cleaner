//! Maintenance tasks. This module holds what both platforms share: the task
//! list and its copy, the run result types and the success messages. How a
//! task runs (and whether it is offered at all) comes from
//! `maintenance_macos.rs` (`osascript … with administrator privileges` for
//! root tasks) or `maintenance_windows.rs` (Flush DNS only), re-exported here.
//! Nothing here deletes files.

#[cfg(target_os = "macos")]
pub use super::maintenance_macos::*;
#[cfg(windows)]
pub use super::maintenance_windows::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TaskId {
    FreeRam,
    FlushDns,
    RebuildSpotlight,
    ReindexLaunchServices,
    PeriodicScripts,
    ResetQuickLook,
}

impl TaskId {
    /// Display order.
    pub const ALL: [TaskId; 6] = [
        TaskId::FreeRam,
        TaskId::FlushDns,
        TaskId::ResetQuickLook,
        TaskId::ReindexLaunchServices,
        TaskId::PeriodicScripts,
        TaskId::RebuildSpotlight,
    ];

    /// Stable key, used for row ids and in `maintenance.json`.
    pub fn key(self) -> &'static str {
        match self {
            TaskId::FreeRam => "free-ram",
            TaskId::FlushDns => "flush-dns",
            TaskId::RebuildSpotlight => "rebuild-spotlight",
            TaskId::ReindexLaunchServices => "reindex-launch-services",
            TaskId::PeriodicScripts => "periodic-scripts",
            TaskId::ResetQuickLook => "reset-quick-look",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            TaskId::FreeRam => "Free up RAM",
            TaskId::FlushDns => "Flush DNS cache",
            TaskId::RebuildSpotlight => "Rebuild Spotlight index",
            TaskId::ReindexLaunchServices => "Reindex Launch Services",
            TaskId::PeriodicScripts => "Run maintenance scripts",
            TaskId::ResetQuickLook => "Reset Quick Look cache",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            TaskId::FreeRam => "Releases cached files from memory so apps get more free RAM",
            TaskId::FlushDns => "Fixes websites that don't load after a network or DNS change",
            TaskId::RebuildSpotlight => {
                "Fixes missing or wrong search results; re-indexing takes hours"
            }
            TaskId::ReindexLaunchServices => {
                "Fixes duplicate apps in “Open With” and wrong default apps"
            }
            TaskId::PeriodicScripts => "Rotates system logs and clears old temporary files",
            TaskId::ResetQuickLook => "Fixes blank or outdated file previews and thumbnails",
        }
    }

    /// Lucide icon name.
    pub fn icon(self) -> &'static str {
        match self {
            TaskId::FreeRam => "memory-stick",
            TaskId::FlushDns => "globe",
            TaskId::RebuildSpotlight => "search",
            TaskId::ReindexLaunchServices => "app-window",
            TaskId::PeriodicScripts => "calendar-clock",
            TaskId::ResetQuickLook => "eye",
        }
    }

    /// The task keeps the Mac busy long after the command returns.
    pub fn long_running(self) -> bool {
        self == TaskId::RebuildSpotlight
    }

    pub fn confirm_title(self) -> String {
        format!("{}?", self.title())
    }

    pub fn confirm_label(self) -> &'static str {
        match self {
            TaskId::FreeRam => "Free up RAM",
            TaskId::FlushDns => "Flush cache",
            TaskId::RebuildSpotlight => "Rebuild index",
            TaskId::ReindexLaunchServices => "Reindex",
            TaskId::PeriodicScripts => "Run scripts",
            TaskId::ResetQuickLook => "Reset cache",
        }
    }
}

/// Tasks applicable on this computer, in display order.
pub fn available_tasks() -> Vec<TaskId> {
    TaskId::ALL
        .into_iter()
        .filter(|t| t.is_available())
        .collect()
}

/// One process to spawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
}

/// Why a task did not complete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunError {
    /// The user dismissed the administrator prompt.
    Cancelled,
    /// The account could not authorize (not an administrator, or denied).
    NotAuthorized(String),
    Failed(String),
}

impl RunError {
    pub fn message(&self) -> String {
        match self {
            RunError::Cancelled => "The administrator prompt was cancelled.".into(),
            RunError::NotAuthorized(m) | RunError::Failed(m) => m.clone(),
        }
    }
}

/// Result of one spawned process.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Message shown after a successful run. `free_memory` is the free-memory
/// reading (bytes) taken just before and just after Free up RAM. It is shown
/// as measured, not as a claim of how much the task freed: other processes
/// allocate and release memory while it runs.
pub fn success_message(id: TaskId, free_memory: Option<(u64, u64)>) -> String {
    match id {
        TaskId::FreeRam => match free_memory {
            Some((before, after)) => {
                use super::present::memory_bytes;
                format!("Memory purged. Free memory went from {} to {}.", memory_bytes(before), memory_bytes(after))
            }
            None => "Memory purged.".into(),
        },
        TaskId::FlushDns => "DNS cache flushed. New lookups go straight to your DNS server.".into(),
        TaskId::RebuildSpotlight => {
            "Spotlight is rebuilding its index in the background. Search may be incomplete for a few hours.".into()
        }
        TaskId::ReindexLaunchServices => {
            "App registrations refreshed. Relaunch Finder if “Open With” still shows duplicates.".into()
        }
        TaskId::PeriodicScripts => "Daily, weekly and monthly maintenance scripts finished.".into(),
        TaskId::ResetQuickLook => "Quick Look cache cleared. Previews are recreated as you browse.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_unique_and_stable() {
        let mut keys: Vec<_> = TaskId::ALL.iter().map(|t| t.key()).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), TaskId::ALL.len());
        assert_eq!(TaskId::FlushDns.key(), "flush-dns");
    }

    #[test]
    fn success_messages() {
        assert_eq!(
            success_message(TaskId::FreeRam, Some((1 << 30, 3 << 30))),
            "Memory purged. Free memory went from 1.00 GB to 3.00 GB."
        );
        assert_eq!(success_message(TaskId::FreeRam, None), "Memory purged.");
        for t in TaskId::ALL {
            assert!(success_message(t, None).ends_with('.'));
        }
    }
}
