//! Persisted GUI preferences (pane sizes, …).
//!
//! Kept separate from the core `cache.db`: these are GUI-only, presentation
//! settings that the TUI neither reads nor writes, so a small TOML file under
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

/// Default when the settings file omits `sync_interval_secs`. Defined in core so
/// the GUI and TUI share one default; clamped to `MIN_SYNC_INTERVAL_SECS` by the
/// engine.
fn default_sync_interval_secs() -> u64 {
    glauca_core::engine::DEFAULT_SYNC_INTERVAL_SECS
}

// `Default` is implemented manually rather than derived: a derived default would
// give `sync_interval_secs = 0` (an invalid interval), so it must fall back to
// the same value as the serde default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuiSettings {
    /// Pixel widths of the resizable panes, left-to-right. Empty until the
    /// user drags a divider for the first time.
    #[serde(default)]
    pub pane_sizes: Vec<f32>,
    /// Color theme. Defaults to `System` (follow the OS appearance).
    #[serde(default)]
    pub theme: ThemePreference,
    /// Whether desktop notifications fire when a background sync surfaces new or
    /// updated items. Defaults to `false` (opt-in).
    #[serde(default)]
    pub notifications_enabled: bool,
    /// Background auto-sync interval (seconds). Defaults to
    /// `DEFAULT_SYNC_INTERVAL_SECS`; the engine clamps it to a sane minimum.
    #[serde(default = "default_sync_interval_secs")]
    pub sync_interval_secs: u64,
}

impl Default for GuiSettings {
    fn default() -> Self {
        Self {
            pane_sizes: Vec::new(),
            theme: ThemePreference::default(),
            notifications_enabled: false,
            sync_interval_secs: default_sync_interval_secs(),
        }
    }
}

impl GuiSettings {
    /// `~/.config/glauca/gui.toml` (or the platform equivalent); falls back to
    /// the local data dir if no config dir is available.
    fn path() -> Option<PathBuf> {
        let base = dirs::config_dir().or_else(dirs::data_local_dir)?;
        Some(base.join("glauca").join("gui.toml"))
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
    fn defaults_to_system_theme() {
        assert_eq!(GuiSettings::default().theme, ThemePreference::System);
        assert_eq!(ThemePreference::default(), ThemePreference::System);
    }

    #[test]
    fn legacy_settings_without_theme_load_as_system() {
        // Older files predate the `theme`/`notifications_enabled` fields;
        // `#[serde(default)]` must fill them.
        let s: GuiSettings = toml::from_str("pane_sizes = [280.0, 0.0, 440.0]").unwrap();
        assert_eq!(s.theme, ThemePreference::System);
        assert!(!s.notifications_enabled);
        assert_eq!(s.pane_sizes, vec![280.0, 0.0, 440.0]);
        // Absent `sync_interval_secs` must fall back to the shared default, not 0.
        assert_eq!(
            s.sync_interval_secs,
            glauca_core::engine::DEFAULT_SYNC_INTERVAL_SECS
        );
    }

    #[test]
    fn sync_interval_defaults_and_round_trips() {
        assert_eq!(
            GuiSettings::default().sync_interval_secs,
            glauca_core::engine::DEFAULT_SYNC_INTERVAL_SECS
        );
        let settings = GuiSettings {
            sync_interval_secs: 120,
            ..Default::default()
        };
        let serialized = toml::to_string(&settings).unwrap();
        let back: GuiSettings = toml::from_str(&serialized).unwrap();
        assert_eq!(back.sync_interval_secs, 120);
    }

    #[test]
    fn notifications_default_off_and_round_trip() {
        assert!(!GuiSettings::default().notifications_enabled);
        let settings = GuiSettings {
            notifications_enabled: true,
            ..Default::default()
        };
        let serialized = toml::to_string(&settings).unwrap();
        assert!(
            serialized.contains("notifications_enabled = true"),
            "expected the flag in output, got:\n{serialized}"
        );
        let back: GuiSettings = toml::from_str(&serialized).unwrap();
        assert!(back.notifications_enabled);
    }

    #[test]
    fn theme_round_trips_as_lowercase() {
        // TOML requires a table at the top level, so round-trip the whole struct
        // rather than a bare enum value.
        let settings = GuiSettings {
            theme: ThemePreference::Dark,
            ..Default::default()
        };
        let serialized = toml::to_string(&settings).unwrap();
        assert!(
            serialized.contains(r#"theme = "dark""#),
            "expected lowercase theme key, got:\n{serialized}"
        );
        let back: GuiSettings = toml::from_str(&serialized).unwrap();
        assert_eq!(back.theme, ThemePreference::Dark);
    }
}
