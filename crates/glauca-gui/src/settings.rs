//! Persisted GUI preferences (pane sizes, …).
//!
//! Kept separate from the core `cache.db`: these are GUI-only, presentation
//! settings that the TUI neither reads nor writes, so a small JSON file under
//! the user config dir is simpler than a DB table. Reads/writes are
//! best-effort — a missing or corrupt file falls back to defaults, and write
//! failures are swallowed (a non-persisted layout is not worth crashing over).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which color theme the GUI uses. `System` (the default) follows the OS
/// appearance; `Light`/`Dark` pin an explicit mode chosen from the View menu.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreference {
    /// Follow the OS dark/light appearance (also the value for older settings
    /// files written before this field existed).
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuiSettings {
    /// Pixel widths of the resizable panes, left-to-right. Empty until the
    /// user drags a divider for the first time.
    #[serde(default)]
    pub pane_sizes: Vec<f32>,
    /// Color theme. Defaults to `System` (follow the OS appearance).
    #[serde(default)]
    pub theme: ThemePreference,
}

impl GuiSettings {
    /// `~/.config/glauca/gui.json` (or the platform equivalent); falls back to
    /// the local data dir if no config dir is available.
    fn path() -> Option<PathBuf> {
        let base = dirs::config_dir().or_else(dirs::data_local_dir)?;
        Some(base.join("glauca").join("gui.json"))
    }

    /// Load saved settings, or defaults if the file is missing/unreadable/corrupt.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
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
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_system_theme() {
        assert_eq!(GuiSettings::default().theme, ThemePreference::System);
        assert_eq!(ThemePreference::default(), ThemePreference::System);
    }

    #[test]
    fn legacy_settings_without_theme_load_as_system() {
        // Older files predate the `theme` field; `#[serde(default)]` must fill it.
        let s: GuiSettings = serde_json::from_str(r#"{"pane_sizes":[280.0,0.0,440.0]}"#).unwrap();
        assert_eq!(s.theme, ThemePreference::System);
        assert_eq!(s.pane_sizes, vec![280.0, 0.0, 440.0]);
    }

    #[test]
    fn theme_round_trips_as_lowercase() {
        let json = serde_json::to_string(&ThemePreference::Dark).unwrap();
        assert_eq!(json, r#""dark""#);
        let back: ThemePreference = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ThemePreference::Dark);
    }
}
