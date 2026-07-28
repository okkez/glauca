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

use glauca_core::actions::CustomActions;
use glauca_core::engine::{EngineCommand, ReviewEvent, load_left_pane_entries};
use glauca_core::filter::{FilterQuery, StreamFilter};
use glauca_core::logic::{ChangeCounts, compute_unread_counts, count_changes, expand_me};
use glauca_core::types::{ItemEntry, LeftPaneEntry, MergeStrategy};
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
    /// User-defined actions from actions.toml, loaded once at startup (same
    /// timing as the TUI/GUI). JS refers to them by name (see
    /// [`list_custom_actions`]); the definitions never cross the IPC boundary.
    pub custom_actions: CustomActions,
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

/// Quit the app (Glauca > Quit). A command rather than a window close from JS,
/// because the `core:default` capability doesn't include window-close
/// permissions; this also mirrors how the GUI quits through its own action.
#[tauri::command]
pub fn quit(app: tauri::AppHandle) {
    app.exit(0);
}

/// Rebuild the left-pane entries (root queries interleaved with their filter
/// streams) from the DB. The front-end calls this after any structural change
/// (add/edit/delete/reorder); the ordering logic lives in
/// `glauca_core::engine::load_left_pane_entries`, shared with `Engine::start`,
/// so it is never re-implemented per front-end.
#[tauri::command]
pub async fn list_entries(state: State<'_, AppState>) -> Result<Vec<LeftPaneEntry>, String> {
    let entries = load_left_pane_entries(&state.pool)
        .await
        .map_err(|e| e.to_string())?;
    // Keep the notification query-name map in sync with the latest labels.
    // Recover from poisoning: the map is plain data, consistent regardless.
    *state
        .query_names
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = query_name_map(&entries);
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
/// interval change takes effect on the next launch (the engine is already
/// running). Takes the whole [`TauriSettings`] struct — serde already defines
/// the field names and defaults, so adding a setting touches only settings.rs
/// and the front-end sends its settings object through unchanged.
#[tauri::command]
pub fn save_settings(state: State<'_, AppState>, settings: TauriSettings) -> Result<(), String> {
    // Persist first, then flip the in-memory flag — so if the write fails the
    // runtime flag and the on-disk file stay in agreement instead of diverging for
    // the rest of the session.
    settings.save().map_err(|e| e.to_string())?;
    state
        .notifications_enabled
        .store(settings.notifications_enabled, Ordering::Relaxed);
    Ok(())
}

/// Compute per-entry unread counts for the entries under `query_id`, reusing
/// `glauca_core::logic::compute_unread_counts` (the same logic the TUI/GUI use)
/// so filter-stream scoping and the Jasper-style unread definition
/// (`updated_at > last_read_updated_at`, per item) stay consistent across
/// front-ends. `items` is the front-end's in-memory list for the query (with
/// up-to-date `last_read_updated_at`), matching how the TUI recomputes.
// These three commands are `async` on purpose: Tauri runs async commands on the
// runtime thread pool, while synchronous commands run on the main/UI thread. Their
// bodies are CPU-bound (serialize the whole item list from JS, then filter/count),
// and `filter_items` fires per filter keystroke — running them on the UI thread
// would jank input on large queries. They don't await anything; `async` alone is
// what moves them off the UI thread.
#[tauri::command]
pub async fn unread_counts(
    state: State<'_, AppState>,
    entries: Vec<LeftPaneEntry>,
    query_id: i64,
    items: Vec<ItemEntry>,
) -> Result<Vec<UnreadCount>, String> {
    Ok(
        compute_unread_counts(&entries, query_id, &items, state.current_user.as_deref())
            .into_iter()
            .map(|((is_filter_stream, entry_id), count)| UnreadCount {
                is_filter_stream,
                entry_id,
                count,
            })
            .collect(),
    )
}

/// One piece of an item title, pre-split on the inline filter's match
/// boundaries so the front-end can paint the highlight without any offset
/// arithmetic. `FilterQuery::highlight_ranges` speaks UTF-8 byte offsets,
/// which JS strings can't index safely — the conversion happens here so that
/// knowledge never crosses the IPC boundary.
#[derive(Serialize)]
pub struct TitleSegment {
    pub text: String,
    pub highlighted: bool,
}

/// One match from [`filter_items`]: the item's index into the input list plus
/// its title split into highlighted / plain segments (empty when the inline
/// filter is empty — the front-end then renders the title as-is).
#[derive(Serialize)]
pub struct FilteredItem {
    pub index: usize,
    pub title_segments: Vec<TitleSegment>,
}

/// Split `title` on `ranges` (sorted, non-overlapping, char-boundary-snapped
/// byte ranges from `FilterQuery::highlight_ranges`) into plain/highlighted
/// segments.
fn split_title(title: &str, ranges: &[(usize, usize)]) -> Vec<TitleSegment> {
    let mut out = Vec::new();
    let mut pos = 0;
    for &(start, end) in ranges {
        if start > pos {
            out.push(TitleSegment {
                text: title[pos..start].to_string(),
                highlighted: false,
            });
        }
        out.push(TitleSegment {
            text: title[start..end].to_string(),
            highlighted: true,
        });
        pos = end;
    }
    if pos < title.len() {
        out.push(TitleSegment {
            text: title[pos..].to_string(),
            highlighted: false,
        });
    }
    out
}

/// Return the entries of `items` that match the selected entry's filter: the
/// filter-stream filter (`stream_filter`, `None` for a root query) ANDed with the
/// inline search-box text (`inline_filter`). Reuses `glauca_core::filter` and
/// `expand_me` so the matching semantics (`state:`/`author:`/`label:`/… plus
/// plain text, `@me` expansion) match the TUI/GUI exactly. Indices (not items)
/// are returned so the front-end keeps its own item objects, preserving the
/// `last_read_updated_at` values it advances locally on read.
#[tauri::command]
pub async fn filter_items(
    state: State<'_, AppState>,
    items: Vec<ItemEntry>,
    stream_filter: Option<String>,
    inline_filter: String,
) -> Result<Vec<FilteredItem>, String> {
    let su = state.current_user.as_deref();
    let stream_q = stream_filter.as_deref().map(|s| StreamFilter::parse(s, su));
    let inline_q = FilterQuery::parse(&expand_me(su, &inline_filter));
    Ok(items
        .iter()
        .enumerate()
        .filter(|(_, it)| {
            stream_q.as_ref().is_none_or(|q| q.matches(it))
                && (inline_q.is_empty() || inline_q.matches(it))
        })
        .map(|(index, it)| FilteredItem {
            index,
            // Only the inline (search-box) filter highlights, like the GUI;
            // stream filters describe the list, not a search.
            title_segments: if inline_q.is_empty() {
                Vec::new()
            } else {
                split_title(&it.title, &inline_q.highlight_ranges(&it.title))
            },
        })
        .collect())
}

/// What the banner needs to know about a background sync's results.
///
/// Carries the rendered `label` rather than leaving the JS to format one, so the
/// wording lives only in `ChangeCounts::banner_label` and can't drift between the
/// three front-ends. `total` is likewise pre-computed so the "is there anything to
/// show?" test is core's `is_empty`, not a re-derivation in JS.
#[derive(serde::Serialize)]
pub struct ItemChanges {
    pub updated: usize,
    pub removed: usize,
    pub total: usize,
    pub label: String,
}

/// Diff `fresh` against `current`, delegating to `glauca_core::logic::count_changes`
/// so the change banner uses the exact definition the TUI/GUI use instead of a JS
/// re-implementation. Removals are part of the result, so a sync that only pruned
/// items no longer matching the query is not mistaken for "nothing changed".
#[tauri::command]
pub async fn count_item_changes(current: Vec<ItemEntry>, fresh: Vec<ItemEntry>) -> ItemChanges {
    let counts: ChangeCounts = count_changes(&current, &fresh);
    ItemChanges {
        updated: counts.updated,
        removed: counts.removed,
        total: counts.total(),
        label: counts.banner_label(),
    }
}

#[tauri::command]
pub async fn load_cached(state: State<'_, AppState>, query_id: i64) -> Result<(), String> {
    dispatch(&state.tx, EngineCommand::LoadCached { query_id }).await
}

#[tauri::command]
pub async fn sync(
    state: State<'_, AppState>,
    query_id: i64,
    query_str: String,
) -> Result<(), String> {
    dispatch(
        &state.tx,
        EngineCommand::Sync {
            query_id,
            query_str,
        },
    )
    .await
}

#[tauri::command]
pub async fn full_resync(
    state: State<'_, AppState>,
    query_id: i64,
    query_str: String,
) -> Result<(), String> {
    dispatch(
        &state.tx,
        EngineCommand::FullResync {
            query_id,
            query_str,
        },
    )
    .await
}

#[tauri::command]
pub async fn sync_if_stale(
    state: State<'_, AppState>,
    query_id: i64,
    query_str: String,
) -> Result<(), String> {
    dispatch(
        &state.tx,
        EngineCommand::SyncIfStale {
            query_id,
            query_str,
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
) -> Result<(), String> {
    dispatch(
        &state.tx,
        EngineCommand::RefreshItem {
            query_id,
            repo_owner,
            repo_name,
            number,
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

/// A custom action as exposed to the front-end: just enough to render a picker.
/// The command template and env stay on the Rust side — JS runs an action by
/// `name` via [`run_custom_action`], so user-defined command lines never enter
/// the webview.
#[derive(Serialize)]
pub struct CustomActionInfo {
    pub name: String,
    pub label: String,
}

/// Custom actions applicable to an item kind (`pull_request` / `issue`), in
/// definition order. Empty when actions.toml is missing or defines none.
#[tauri::command]
pub fn list_custom_actions(state: State<'_, AppState>, kind: String) -> Vec<CustomActionInfo> {
    state
        .custom_actions
        .for_kind(&kind)
        .into_iter()
        .map(|a| CustomActionInfo {
            name: a.name.clone(),
            label: a.display_label().to_string(),
        })
        .collect()
}

/// Run the custom action `name` on `item`. The action is resolved from the
/// startup-loaded actions.toml (kind-checked again here); the engine executes it
/// and reports the result via ActionDone / ActionError.
#[tauri::command]
pub async fn run_custom_action(
    state: State<'_, AppState>,
    name: String,
    item: ItemEntry,
) -> Result<(), String> {
    let action = state
        .custom_actions
        .for_kind(&item.kind)
        .into_iter()
        .find(|a| a.name == name)
        .cloned()
        .ok_or_else(|| format!("unknown custom action: {name}"))?;
    dispatch(
        &state.tx,
        EngineCommand::RunCustomAction {
            action: Box::new(action),
            item: Box::new(item),
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
