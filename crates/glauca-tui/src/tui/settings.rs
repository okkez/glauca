//! Persisted TUI preferences.
//!
//! Mirrors the GUI's `gui.toml` (see `glauca-gui/src/settings.rs`) but kept
//! separate and per-front-end: a small TOML file under the user config dir,
//! holding only TUI-relevant settings. Reads/writes are best-effort — a missing
//! or corrupt file falls back to defaults, and write failures are swallowed.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TuiSettings {
    /// Whether desktop notifications fire when a background sync surfaces new or
    /// updated items. Defaults to `false` (opt-in).
    #[serde(default)]
    pub notifications_enabled: bool,
}

impl TuiSettings {
    /// `~/.config/glauca/tui.toml` (or the platform equivalent); falls back to
    /// the local data dir if no config dir is available.
    fn path() -> Option<PathBuf> {
        let base = dirs::config_dir().or_else(dirs::data_local_dir)?;
        Some(base.join("glauca").join("tui.toml"))
    }

    /// Load saved settings, or defaults if the file is missing/unreadable/corrupt.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Persist settings, creating the parent directory if needed. Best-effort:
    /// any I/O or serialization error is ignored.
    pub fn save(&self) {
        let Some(path) = Self::path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(serialized) = toml::to_string_pretty(self) {
            let _ = std::fs::write(&path, serialized);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifications_default_off() {
        assert!(!TuiSettings::default().notifications_enabled);
    }

    #[test]
    fn legacy_empty_file_loads_as_off() {
        // A file predating the field (or an empty one) must fill the default.
        let s: TuiSettings = toml::from_str("").unwrap();
        assert!(!s.notifications_enabled);
    }

    #[test]
    fn notifications_round_trip() {
        let settings = TuiSettings {
            notifications_enabled: true,
        };
        let serialized = toml::to_string(&settings).unwrap();
        assert!(
            serialized.contains("notifications_enabled = true"),
            "expected the flag in output, got:\n{serialized}"
        );
        let back: TuiSettings = toml::from_str(&serialized).unwrap();
        assert!(back.notifications_enabled);
    }
}
