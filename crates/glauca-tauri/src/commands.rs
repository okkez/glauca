//! Tauri command handlers: the front-end → engine half of the IPC bridge.
//!
//! Each command takes primitive arguments from JavaScript, builds the matching
//! [`EngineCommand`], and forwards it on the engine's command channel. The engine
//! replies asynchronously via `AppMessage`, which `main.rs` streams back to the
//! front-end over the `app-message` event (the engine → front-end half).
//!
//! The whole `EngineCommand` enum is never exposed to JS; the variant is chosen
//! here so the front-end only deals with plain values.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use glauca_core::db;
use glauca_core::engine::{EngineCommand, ReviewEvent};
use glauca_core::filter::FilterQuery;
use glauca_core::logic::{compute_unread_counts, expand_me};
use glauca_core::types::{FilterStreamEntry, ItemEntry, LeftPaneEntry, MergeStrategy, QueryEntry};
use serde::Serialize;
use sqlx::SqlitePool;
use tauri::State;
use tokio::sync::mpsc::Sender;

use crate::settings::TauriSettings;

/// Shared state held by Tauri.
pub struct AppState {
    pub tx: Sender<EngineCommand>,
    /// Pre-serialized initial state (left-pane entries + current user) for `init`.
    pub init: serde_json::Value,
    /// Authenticated login, to expand `@me` when computing unread counts.
    pub current_user: Option<String>,
    /// DB pool, to rebuild the left pane after structural changes.
    pub pool: SqlitePool,
    /// Whether desktop notifications fire (toggled at runtime via save_settings;
    /// read by the engine-message loop in main.rs through a shared clone).
    pub notifications_enabled: Arc<AtomicBool>,
    /// Root-query id -> display label, for notification text. Refreshed on
    /// startup and by list_entries.
    pub query_names: Arc<Mutex<HashMap<i64, String>>>,
}

/// One left-pane entry's unread count, as returned by [`unread_counts`]. Mirrors
/// the `(is_filter_stream, entry_id) -> count` map that `compute_unread_counts`
/// produces, in a shape the front-end can key on directly.
#[derive(Serialize)]
pub struct UnreadCount {
    pub is_filter_stream: bool,
    pub entry_id: i64,
    pub count: usize,
}

/// Forward a command to the engine, mapping a closed channel to a string error
/// (Tauri commands must return a serializable error).
async fn dispatch(tx: &Sender<EngineCommand>, cmd: EngineCommand) -> Result<(), String> {
    tx.send(cmd).await.map_err(|e| e.to_string())
}

/// Return the initial state (left-pane entries + current user) captured at
/// startup. Synchronous: it is just a cached JSON value.
#[tauri::command]
pub fn init(state: State<'_, AppState>) -> serde_json::Value {
    state.init.clone()
}

/// Rebuild the left-pane entries (root queries interleaved with their filter
/// streams) from the DB. The front-end calls this after any structural change
/// (add/edit/delete/reorder) so it never re-implements the ordering logic — this
/// mirrors how `Engine::start` assembles the initial entries.
#[tauri::command]
pub async fn list_entries(state: State<'_, AppState>) -> Result<Vec<LeftPaneEntry>, String> {
    let pool = &state.pool;
    let query_rows = db::list_queries(pool).await.map_err(|e| e.to_string())?;
    let mut entries: Vec<LeftPaneEntry> = Vec::new();
    for r in query_rows {
        let streams = db::list_filter_streams(pool, r.id).await.unwrap_or_default();
        let kind = r.kind.clone();
        let label = r.name.clone().unwrap_or_else(|| r.query.clone());
        entries.push(LeftPaneEntry::Query(QueryEntry {
            id: r.id,
            label,
            query_str: r.query.clone(),
            kind: kind.clone(),
            last_viewed_at: r.last_viewed_at,
        }));
        for s in streams {
            entries.push(LeftPaneEntry::FilterStream(FilterStreamEntry {
                id: s.id,
                parent_id: s.parent_id,
                name: s.name,
                filter: s.filter,
                kind: kind.clone(),
                last_viewed_at: s.last_viewed_at,
            }));
        }
    }
    // Keep the notification query-name map in sync with the latest labels.
    *state.query_names.lock().unwrap() = query_name_map(&entries);
    Ok(entries)
}

/// Build the root-query id -> label map used for notification text.
pub fn query_name_map(entries: &[LeftPaneEntry]) -> HashMap<i64, String> {
    entries
        .iter()
        .filter_map(|e| match e {
            LeftPaneEntry::Query(q) => Some((q.id, q.label.clone())),
            LeftPaneEntry::FilterStream(_) => None,
        })
        .collect()
}

/// Return the persisted settings (theme / notifications / sync interval) so the
/// front-end can render the settings UI.
#[tauri::command]
pub fn get_settings() -> TauriSettings {
    TauriSettings::load()
}

/// Persist settings and apply the notifications flag immediately. The sync
/// interval change takes effect on the next launch (the engine is already running).
#[tauri::command]
pub fn save_settings(
    state: State<'_, AppState>,
    theme: String,
    notifications_enabled: bool,
    sync_interval_secs: u64,
) -> Result<(), String> {
    state
        .notifications_enabled
        .store(notifications_enabled, Ordering::Relaxed);
    TauriSettings {
        theme,
        notifications_enabled,
        sync_interval_secs,
    }
    .save()
    .map_err(|e| e.to_string())
}

/// Compute per-entry unread counts for the entries under `query_id`, reusing
/// `glauca_core::logic::compute_unread_counts` (the same logic the TUI/GUI use)
/// so filter-stream scoping and the "new-since-last-viewed AND unread" definition
/// stay consistent across front-ends. `items` is the front-end's in-memory list
/// for the query (with up-to-date `read` flags), matching how the TUI recomputes.
#[tauri::command]
pub fn unread_counts(
    state: State<'_, AppState>,
    entries: Vec<LeftPaneEntry>,
    query_id: i64,
    items: Vec<ItemEntry>,
) -> Vec<UnreadCount> {
    compute_unread_counts(&entries, query_id, &items, state.current_user.as_deref())
        .into_iter()
        .map(|((is_filter_stream, entry_id), count)| UnreadCount {
            is_filter_stream,
            entry_id,
            count,
        })
        .collect()
}

/// Return the indices of `items` that match the selected entry's filter: the
/// filter-stream filter (`stream_filter`, `None` for a root query) ANDed with the
/// inline search-box text (`inline_filter`). Reuses `glauca_core::filter` and
/// `expand_me` so the matching semantics (`state:`/`author:`/`label:`/… plus
/// plain text, `@me` expansion) match the TUI/GUI exactly. Indices (not items)
/// are returned so the front-end keeps its own item objects, preserving the
/// `read` flags it mutates locally.
#[tauri::command]
pub fn filter_items(
    state: State<'_, AppState>,
    items: Vec<ItemEntry>,
    stream_filter: Option<String>,
    inline_filter: String,
) -> Vec<usize> {
    let su = state.current_user.as_deref();
    let stream_q = stream_filter
        .as_deref()
        .map(|s| FilterQuery::parse(&expand_me(su, s)));
    let inline_q = FilterQuery::parse(&expand_me(su, &inline_filter));
    items
        .iter()
        .enumerate()
        .filter(|(_, it)| {
            stream_q.as_ref().is_none_or(|q| q.matches(it))
                && (inline_q.is_empty() || inline_q.matches(it))
        })
        .map(|(i, _)| i)
        .collect()
}

#[tauri::command]
pub async fn load_cached(
    state: State<'_, AppState>,
    query_id: i64,
    highlight_since: Option<String>,
) -> Result<(), String> {
    dispatch(
        &state.tx,
        EngineCommand::LoadCached {
            query_id,
            highlight_since,
        },
    )
    .await
}

#[tauri::command]
pub async fn sync(
    state: State<'_, AppState>,
    query_id: i64,
    query_str: String,
    highlight_since: Option<String>,
) -> Result<(), String> {
    dispatch(
        &state.tx,
        EngineCommand::Sync {
            query_id,
            query_str,
            highlight_since,
        },
    )
    .await
}

#[tauri::command]
pub async fn full_resync(
    state: State<'_, AppState>,
    query_id: i64,
    query_str: String,
    highlight_since: Option<String>,
) -> Result<(), String> {
    dispatch(
        &state.tx,
        EngineCommand::FullResync {
            query_id,
            query_str,
            highlight_since,
        },
    )
    .await
}

#[tauri::command]
pub async fn sync_if_stale(
    state: State<'_, AppState>,
    query_id: i64,
    query_str: String,
    highlight_since: Option<String>,
) -> Result<(), String> {
    dispatch(
        &state.tx,
        EngineCommand::SyncIfStale {
            query_id,
            query_str,
            highlight_since,
        },
    )
    .await
}

#[tauri::command]
pub async fn refresh_item(
    state: State<'_, AppState>,
    query_id: i64,
    repo_owner: String,
    repo_name: String,
    number: i64,
    highlight_since: Option<String>,
) -> Result<(), String> {
    dispatch(
        &state.tx,
        EngineCommand::RefreshItem {
            query_id,
            repo_owner,
            repo_name,
            number,
            highlight_since,
        },
    )
    .await
}

#[tauri::command]
pub async fn mark_entry_viewed(
    state: State<'_, AppState>,
    entry_id: i64,
    is_filter_stream: bool,
    viewed_at: String,
) -> Result<(), String> {
    dispatch(
        &state.tx,
        EngineCommand::MarkEntryViewed {
            entry_id,
            is_filter_stream,
            viewed_at,
        },
    )
    .await
}

#[tauri::command]
pub async fn enqueue_stale(
    state: State<'_, AppState>,
    skip_query_id: Option<i64>,
) -> Result<(), String> {
    dispatch(&state.tx, EngineCommand::EnqueueStale { skip_query_id }).await
}

#[tauri::command]
pub async fn add_query(
    state: State<'_, AppState>,
    name: Option<String>,
    query: String,
) -> Result<(), String> {
    dispatch(&state.tx, EngineCommand::AddQuery { name, query }).await
}

#[tauri::command]
pub async fn add_filter_stream(
    state: State<'_, AppState>,
    parent_id: i64,
    kind: String,
    name: String,
    filter: String,
) -> Result<(), String> {
    dispatch(
        &state.tx,
        EngineCommand::AddFilterStream {
            parent_id,
            kind,
            name,
            filter,
        },
    )
    .await
}

#[tauri::command]
pub async fn edit_query(
    state: State<'_, AppState>,
    id: i64,
    name: Option<String>,
    query: String,
) -> Result<(), String> {
    dispatch(&state.tx, EngineCommand::EditQuery { id, name, query }).await
}

#[tauri::command]
pub async fn edit_filter_stream(
    state: State<'_, AppState>,
    id: i64,
    name: String,
    filter: String,
) -> Result<(), String> {
    dispatch(
        &state.tx,
        EngineCommand::EditFilterStream { id, name, filter },
    )
    .await
}

#[tauri::command]
pub async fn delete_query(state: State<'_, AppState>, query_id: i64) -> Result<(), String> {
    dispatch(&state.tx, EngineCommand::DeleteQuery { query_id }).await
}

#[tauri::command]
pub async fn delete_filter_stream(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    dispatch(&state.tx, EngineCommand::DeleteFilterStream { id }).await
}

#[tauri::command]
pub async fn swap_query_positions(
    state: State<'_, AppState>,
    upper_id: i64,
    lower_id: i64,
    active_id: i64,
) -> Result<(), String> {
    dispatch(
        &state.tx,
        EngineCommand::SwapQueryPositions {
            upper_id,
            lower_id,
            active_id,
        },
    )
    .await
}

#[tauri::command]
pub async fn swap_filter_stream_positions(
    state: State<'_, AppState>,
    upper_id: i64,
    lower_id: i64,
    active_id: i64,
) -> Result<(), String> {
    dispatch(
        &state.tx,
        EngineCommand::SwapFilterStreamPositions {
            upper_id,
            lower_id,
            active_id,
        },
    )
    .await
}

#[tauri::command]
pub async fn load_comments(
    state: State<'_, AppState>,
    owner: String,
    repo: String,
    number: u64,
) -> Result<(), String> {
    dispatch(
        &state.tx,
        EngineCommand::LoadComments {
            owner,
            repo,
            number,
        },
    )
    .await
}

#[tauri::command]
pub async fn open_browser(state: State<'_, AppState>, item: ItemEntry) -> Result<(), String> {
    dispatch(
        &state.tx,
        EngineCommand::OpenBrowser {
            item: Box::new(item),
        },
    )
    .await
}

#[tauri::command]
pub async fn comment(
    state: State<'_, AppState>,
    url: String,
    kind: String,
    body: String,
) -> Result<(), String> {
    dispatch(&state.tx, EngineCommand::Comment { url, kind, body }).await
}

// `event` / `strategy` deserialize straight into the core enums (which derive
// Deserialize with snake_case renaming), so the JS sends "approve" / "squash"
// etc. and serde is the single source of truth for the valid values.
#[tauri::command]
pub async fn submit_review(
    state: State<'_, AppState>,
    url: String,
    event: ReviewEvent,
    body: Option<String>,
) -> Result<(), String> {
    dispatch(&state.tx, EngineCommand::SubmitReview { url, event, body }).await
}

#[tauri::command]
pub async fn merge(
    state: State<'_, AppState>,
    url: String,
    strategy: MergeStrategy,
) -> Result<(), String> {
    dispatch(&state.tx, EngineCommand::Merge { url, strategy }).await
}

#[tauri::command]
pub async fn mark_item_read(
    state: State<'_, AppState>,
    query_id: i64,
    repo_owner: String,
    repo_name: String,
    number: i64,
) -> Result<(), String> {
    dispatch(
        &state.tx,
        EngineCommand::MarkItemRead {
            query_id,
            repo_owner,
            repo_name,
            number,
        },
    )
    .await
}

#[tauri::command]
pub async fn mark_all_read(
    state: State<'_, AppState>,
    query_id: i64,
    filter: Option<String>,
) -> Result<(), String> {
    // The engine expects an already-`@me`-expanded filter; expand here (reusing
    // core's expand_me) so the front-end can pass the raw filter-stream filter.
    let filter = filter.map(|f| expand_me(state.current_user.as_deref(), &f).into_owned());
    dispatch(&state.tx, EngineCommand::MarkAllRead { query_id, filter }).await
}
