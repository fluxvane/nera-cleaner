//! Persistent user preferences (JSON in Application Support).
//!
//! Read with [`Settings::load`] (never fails: missing/corrupt file gives
//! defaults) and write with [`Settings::save`]. Core functions that depend on a
//! preference take it as an explicit argument rather than reading this file.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeMode {
    pub const ALL: [ThemeMode; 3] = [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark];

    pub fn label(self) -> &'static str {
        match self {
            ThemeMode::System => "System",
            ThemeMode::Light => "Light",
            ThemeMode::Dark => "Dark",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeMode,
    /// Cleanup moves items to the Trash instead of deleting them permanently.
    pub move_to_trash: bool,
    /// Ask for confirmation before any cleanup/delete.
    pub confirm_before_cleanup: bool,
    /// Post a macOS notification when a cleanup finishes.
    pub notify_after_cleanup: bool,
    /// Include dotfiles when scanning.
    pub scan_hidden_files: bool,
    /// Maximum directory depth for recursive size scans.
    pub max_scan_depth: u32,
    /// Run Smart Scan when the app opens.
    pub auto_scan_on_launch: bool,
    /// Extra folders the user never wants touched.
    pub excluded_paths: Vec<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeMode::System,
            move_to_trash: true,
            confirm_before_cleanup: true,
            notify_after_cleanup: true,
            scan_hidden_files: false,
            max_scan_depth: 15,
            auto_scan_on_launch: false,
            excluded_paths: Vec::new(),
        }
    }
}

impl Settings {
    pub fn file_path() -> PathBuf {
        crate::paths::app_support_dir().join("settings.json")
    }

    pub fn load() -> Self {
        std::fs::read(Self::file_path())
            .map(|bytes| Self::from_json(&bytes))
            .unwrap_or_default()
    }

    /// Parse leniently, field by field: a field with a bad value falls back to
    /// its default without discarding the other, valid fields.
    pub fn from_json(bytes: &[u8]) -> Self {
        let Ok(serde_json::Value::Object(file)) =
            serde_json::from_slice::<serde_json::Value>(bytes)
        else {
            return Self::default();
        };
        let Ok(serde_json::Value::Object(mut merged)) = serde_json::to_value(Self::default())
        else {
            return Self::default();
        };
        for (key, value) in file {
            if !merged.contains_key(&key) {
                continue;
            }
            let previous = merged.insert(key.clone(), value);
            if serde_json::from_value::<Self>(serde_json::Value::Object(merged.clone())).is_err() {
                if let Some(previous) = previous {
                    merged.insert(key, previous);
                }
            }
        }
        serde_json::from_value(serde_json::Value::Object(merged)).unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        crate::fs_util::write_atomic(&Self::file_path(), &serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    /// Clamped scan depth (1…50).
    pub fn scan_depth(&self) -> usize {
        self.max_scan_depth.clamp(1, 50) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_and_missing_fields_fall_back_to_defaults() {
        let s: Settings = serde_json::from_str(r#"{"move_to_trash": false, "bogus": 1}"#).unwrap();
        assert!(!s.move_to_trash);
        assert!(s.confirm_before_cleanup);
        assert_eq!(s.theme, ThemeMode::System);
    }

    #[test]
    fn one_bad_field_keeps_the_others() {
        let s = Settings::from_json(
            br#"{"move_to_trash": false, "max_scan_depth": "deep", "theme": "Dark"}"#,
        );
        assert!(!s.move_to_trash);
        assert_eq!(s.max_scan_depth, Settings::default().max_scan_depth);
        assert_eq!(Settings::from_json(b"not json"), Settings::default());
    }
}
