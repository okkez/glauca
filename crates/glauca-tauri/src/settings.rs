//! Persisted glauca-tauri preferences.
//!
//! Mirrors the per-front-end settings pattern of `glauca-tui` (`tui.toml`) and
//! `glauca-gui` (`gui.toml`): a small TOML file under the user config dir, read
//! best-effort (a missing or corrupt file falls back to defaults).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Default when the settings file omits `sync_interval_secs`. Shares the core
/// constant with the other front-ends; the engine clamps it to a sane minimum.
fn default_sync_interval_secs() -> u64 {
    glauca_core::engine::DEFAULT_SYNC_INTERVAL_SECS
}

// `Default` is implemented manually rather than derived: a derived default would
// give `sync_interval_secs = 0` (an invalid interval), so it must fall back to
// the same value as the serde default.
fn default_theme() -> String {
    "system".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TauriSettings {
    /// Background auto-sync interval (seconds). Defaults to
    /// `DEFAULT_SYNC_INTERVAL_SECS`; the engine clamps it to a sane minimum.
    #[serde(default = "default_sync_interval_secs")]
    pub sync_interval_secs: u64,
    /// UI theme preference: "system" | "light" | "dark".
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Whether desktop notifications fire when a background sync surfaces new or
    /// updated items. Defaults to `false` (opt-in), like the TUI/GUI.
    #[serde(default)]
    pub notifications_enabled: bool,
    /// Persisted `(sidebar, detail)` pane widths in px, like the GUI's
    /// `pane_sizes`. `None` until the user first drags a divider.
    #[serde(default)]
    pub pane_sizes: Option<(f64, f64)>,
}

impl Default for TauriSettings {
    fn default() -> Self {
        Self {
            sync_interval_secs: default_sync_interval_secs(),
            theme: default_theme(),
            notifications_enabled: false,
            pane_sizes: None,
        }
    }
}

impl TauriSettings {
    /// `~/.config/glauca/tauri.toml` (or the platform equivalent); falls back to
    /// the local data dir if no config dir is available.
    fn path() -> Option<PathBuf> {
        let base = dirs::config_dir().or_else(dirs::data_local_dir)?;
        Some(base.join("glauca").join("tauri.toml"))
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

    /// Persist settings to `tauri.toml` via `glauca_core::fs::atomic_write`
    /// (temp file + rename; see there for why). Propagates I/O and serialization
    /// errors to the caller.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path().ok_or_else(|| anyhow::anyhow!("no config dir"))?;
        glauca_core::fs::atomic_write(&path, toml::to_string_pretty(self)?)?;
        Ok(())
    }
}
