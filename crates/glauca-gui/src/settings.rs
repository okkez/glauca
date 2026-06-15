//! Persisted GUI preferences (pane sizes, …).
//!
//! Kept separate from the core `cache.db`: these are GUI-only, presentation
//! settings that the TUI neither reads nor writes, so a small JSON file under
//! the user config dir is simpler than a DB table. Reads/writes are
//! best-effort — a missing or corrupt file falls back to defaults, and write
//! failures are swallowed (a non-persisted layout is not worth crashing over).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuiSettings {
    /// Pixel widths of the resizable panes, left-to-right. Empty until the
    /// user drags a divider for the first time.
    #[serde(default)]
    pub pane_sizes: Vec<f32>,
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
