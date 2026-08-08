//! Persisted glauca-tauri preferences.
//!
//! A small TOML file under the user config dir, read best-effort: a missing or corrupt
//! file falls back to defaults.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

fn default_sync_interval_secs() -> u64 {
    glauca_core::engine::DEFAULT_SYNC_INTERVAL_SECS
}

fn default_full_fetch_interval_secs() -> u64 {
    glauca_core::engine::DEFAULT_FULL_FETCH_INTERVAL_SECS
}

fn default_retention_days() -> u64 {
    glauca_core::engine::DEFAULT_RETENTION_DAYS
}

fn default_max_items_per_query() -> u64 {
    glauca_core::engine::DEFAULT_MAX_ITEMS_PER_QUERY
}

fn default_theme() -> String {
    "system".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TauriSettings {
    /// Background auto-sync interval (seconds); the engine clamps it to a minimum.
    #[serde(default = "default_sync_interval_secs")]
    pub sync_interval_secs: u64,
    /// How often an incremental background sync is upgraded to a full fetch, which is what
    /// prunes items that silently stopped matching. Lower it to drop such items sooner,
    /// raise it to spend less API quota on large queries.
    #[serde(default = "default_full_fetch_interval_secs")]
    pub full_fetch_interval_secs: u64,
    /// UI theme preference: "system" | "light" | "dark".
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Whether desktop notifications fire when a background sync surfaces new or updated
    /// items. Opt-in.
    #[serde(default)]
    pub notifications_enabled: bool,
    /// Persisted `(sidebar, detail)` pane widths in px. `None` until the user first drags
    /// a divider.
    #[serde(default)]
    pub pane_sizes: Option<(f64, f64)>,
    /// Age (days) past which a cached item's re-fetchable `body` is cleared; terminal-state
    /// items are cleared regardless of age.
    #[serde(default = "default_retention_days")]
    pub retention_days: u64,
    /// Per-query cap on cached rows; read overflow beyond it is pruned. Raised to GitHub
    /// search's ~1000-result cap if set lower, since a smaller cap deletes rows the next
    /// sync re-inserts as unread.
    #[serde(default = "default_max_items_per_query")]
    pub max_items_per_query: u64,
}

// Hand-written rather than derived: a derived default would give
// `sync_interval_secs = 0`, an invalid interval, instead of the serde default.
impl Default for TauriSettings {
    fn default() -> Self {
        Self {
            sync_interval_secs: default_sync_interval_secs(),
            full_fetch_interval_secs: default_full_fetch_interval_secs(),
            theme: default_theme(),
            notifications_enabled: false,
            pane_sizes: None,
            retention_days: default_retention_days(),
            max_items_per_query: default_max_items_per_query(),
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

    /// Persist settings to `tauri.toml` via `glauca_core::fs::atomic_write`.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path().ok_or_else(|| anyhow::anyhow!("no config dir"))?;
        glauca_core::fs::atomic_write(&path, toml::to_string_pretty(self)?)?;
        Ok(())
    }
}
