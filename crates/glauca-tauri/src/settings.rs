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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TauriSettings {
    /// Background auto-sync interval (seconds). Defaults to
    /// `DEFAULT_SYNC_INTERVAL_SECS`; the engine clamps it to a sane minimum.
    #[serde(default = "default_sync_interval_secs")]
    pub sync_interval_secs: u64,
}

impl Default for TauriSettings {
    fn default() -> Self {
        Self {
            sync_interval_secs: default_sync_interval_secs(),
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
}
