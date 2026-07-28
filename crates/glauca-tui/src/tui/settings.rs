//! Persisted TUI preferences.
//!
//! Mirrors the GUI's `gui.toml` (see `glauca-gui/src/settings.rs`) but kept
//! separate and per-front-end: a small TOML file under the user config dir,
//! holding only TUI-relevant settings. Reads/writes are best-effort — a missing
//! or corrupt file falls back to defaults, and write failures are swallowed.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Default when the settings file omits `sync_interval_secs`. Defined in core so
/// the GUI and TUI share one default; clamped to `MIN_SYNC_INTERVAL_SECS` by the
/// engine.
fn default_sync_interval_secs() -> u64 {
    glauca_core::engine::DEFAULT_SYNC_INTERVAL_SECS
}

/// Default when the settings file omits `full_fetch_interval_secs`. Defined in core
/// so all front-ends share one default; clamped to at least the sync interval by
/// `SyncConfig::effective`.
fn default_full_fetch_interval_secs() -> u64 {
    glauca_core::engine::DEFAULT_FULL_FETCH_INTERVAL_SECS
}

/// Default when the settings file omits `retention_days`. Defined in core so all
/// front-ends share one default. See `db::clear_stale_bodies`.
fn default_retention_days() -> u64 {
    glauca_core::engine::DEFAULT_RETENTION_DAYS
}

/// Default when the settings file omits `max_items_per_query`. See
/// `db::prune_query_overflow`.
fn default_max_items_per_query() -> u64 {
    glauca_core::engine::DEFAULT_MAX_ITEMS_PER_QUERY
}

// `Default` is implemented manually rather than derived: a derived default would
// give `sync_interval_secs = 0` (an invalid interval), so it must fall back to
// the same value as the serde default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiSettings {
    /// Whether desktop notifications fire when a background sync surfaces new or
    /// updated items. Defaults to `false` (opt-in).
    #[serde(default)]
    pub notifications_enabled: bool,
    /// Background auto-sync interval (seconds). Defaults to
    /// `DEFAULT_SYNC_INTERVAL_SECS`; the engine clamps it to a sane minimum.
    #[serde(default = "default_sync_interval_secs")]
    pub sync_interval_secs: u64,
    /// How often an incremental background sync is upgraded to a full fetch, which
    /// is what prunes items that silently stopped matching the query. Lower it to
    /// drop such items sooner, raise it to spend less API quota on large queries.
    /// Defaults to `DEFAULT_FULL_FETCH_INTERVAL_SECS`.
    #[serde(default = "default_full_fetch_interval_secs")]
    pub full_fetch_interval_secs: u64,
    /// Whether to render semantic icons using icon-font glyphs (Font Awesome /
    /// Nerd Font) instead of the default emoji/Unicode set. Defaults to `false`
    /// (opt-in): these glyphs only display in a terminal whose font provides
    /// them (e.g. `fonts-font-awesome`, or a Nerd Font).
    #[serde(default)]
    pub use_icon_font: bool,
    /// Age (days) past which a cached item's re-fetchable `body` is cleared to
    /// reclaim cache space (terminal-state items are cleared regardless of age).
    /// Defaults to `DEFAULT_RETENTION_DAYS`.
    #[serde(default = "default_retention_days")]
    pub retention_days: u64,
    /// Per-query cap on cached rows; read overflow beyond it is pruned. Defaults
    /// to `DEFAULT_MAX_ITEMS_PER_QUERY`.
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

    /// Persist settings via `glauca_core::fs::atomic_write` (temp file + rename;
    /// see there for why). Best-effort: any I/O or serialization error is ignored.
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
