//! Persisted TUI preferences.
//!
//! A small TOML file under the user config dir, holding only TUI-relevant settings. Reads
//! and writes are best-effort: a missing or corrupt file falls back to defaults, and write
//! failures are swallowed.

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

// `Default` is hand-written below rather than derived: a derived default would give
// `sync_interval_secs = 0`, an invalid interval, instead of the serde default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiSettings {
    /// Whether desktop notifications fire when a background sync surfaces new or updated
    /// items. Opt-in.
    #[serde(default)]
    pub notifications_enabled: bool,
    /// Background auto-sync interval (seconds); the engine clamps it to a minimum.
    #[serde(default = "default_sync_interval_secs")]
    pub sync_interval_secs: u64,
    /// How often an incremental background sync is upgraded to a full fetch, which is what
    /// prunes items that silently stopped matching. Lower it to drop such items sooner,
    /// raise it to spend less API quota on large queries.
    #[serde(default = "default_full_fetch_interval_secs")]
    pub full_fetch_interval_secs: u64,
    /// Whether to render semantic icons as icon-font glyphs (Font Awesome / Nerd Font)
    /// instead of the emoji/Unicode set. Opt-in: these only display in a terminal whose
    /// font provides them.
    #[serde(default)]
    pub use_icon_font: bool,
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

impl Default for TuiSettings {
    fn default() -> Self {
        Self {
            notifications_enabled: false,
            sync_interval_secs: default_sync_interval_secs(),
            full_fetch_interval_secs: default_full_fetch_interval_secs(),
            use_icon_font: false,
            retention_days: default_retention_days(),
            max_items_per_query: default_max_items_per_query(),
        }
    }
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

    /// Persist settings via `glauca_core::fs::atomic_write`. Best-effort: any I/O or
    /// serialization error is ignored.
    pub fn save(&self) {
        let Some(path) = Self::path() else {
            return;
        };
        if let Ok(serialized) = toml::to_string_pretty(self) {
            let _ = glauca_core::fs::atomic_write(&path, serialized);
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
    fn icon_font_default_off() {
        assert!(!TuiSettings::default().use_icon_font);
    }

    #[test]
    fn icon_font_round_trip() {
        let settings = TuiSettings {
            use_icon_font: true,
            ..Default::default()
        };
        let serialized = toml::to_string(&settings).unwrap();
        assert!(
            serialized.contains("use_icon_font = true"),
            "expected the flag in output, got:\n{serialized}"
        );
        let back: TuiSettings = toml::from_str(&serialized).unwrap();
        assert!(back.use_icon_font);
    }

    #[test]
    fn legacy_empty_file_loads_as_off() {
        // A file predating the fields (or an empty one) must fill the defaults.
        let s: TuiSettings = toml::from_str("").unwrap();
        assert!(!s.notifications_enabled);
        assert!(!s.use_icon_font);
        assert_eq!(
            s.sync_interval_secs,
            glauca_core::engine::DEFAULT_SYNC_INTERVAL_SECS
        );
    }

    #[test]
    fn notifications_round_trip() {
        let settings = TuiSettings {
            notifications_enabled: true,
            ..Default::default()
        };
        let serialized = toml::to_string(&settings).unwrap();
        assert!(
            serialized.contains("notifications_enabled = true"),
            "expected the flag in output, got:\n{serialized}"
        );
        let back: TuiSettings = toml::from_str(&serialized).unwrap();
        assert!(back.notifications_enabled);
    }

    #[test]
    fn sync_interval_defaults_and_round_trips() {
        assert_eq!(
            TuiSettings::default().sync_interval_secs,
            glauca_core::engine::DEFAULT_SYNC_INTERVAL_SECS
        );
        let settings = TuiSettings {
            sync_interval_secs: 120,
            ..Default::default()
        };
        let serialized = toml::to_string(&settings).unwrap();
        let back: TuiSettings = toml::from_str(&serialized).unwrap();
        assert_eq!(back.sync_interval_secs, 120);
    }
}
