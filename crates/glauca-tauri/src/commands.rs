//! Tauri command handlers: the front-end → engine half of the IPC bridge.
//!
//! Each command takes primitive arguments from JavaScript, builds the matching
//! [`EngineCommand`], and forwards it on the engine's command channel. The engine replies
//! asynchronously via `AppMessage`, which `main.rs` streams back over the `app-message`
//! event. The `EngineCommand` enum itself never crosses into JS.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use glauca_core::actions::CustomActions;
use glauca_core::engine::{EngineCommand, EngineInit, ReviewEvent, load_left_pane_entries};
use glauca_core::filter::{FilterQuery, StreamFilter};
use glauca_core::logic::{
    ChangeCounts, ME_UNEXPANDED_WARNING, compute_unread_counts, count_changes, expand_me,
    has_unexpanded_me,
};
use glauca_core::types::{ItemEntry, LeftPaneEntry, MergeStrategy};
use serde::Serialize;
use sqlx::SqlitePool;
use tauri::State;
use tokio::sync::mpsc::Sender;

use crate::settings::TauriSettings;

/// What is currently known about the authenticated user. All three fields are `Option`
/// because the lookup can fail — `login: None` is "not resolved yet" — and because
/// `name`/`avatar_url` may simply be unset on the account.
///
/// Kept as a unit rather than three cells so `init` hands back a consistent trio.
#[derive(Default, Clone)]
pub struct CurrentUserState {
    pub login: Option<String>,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
}

/// Shared state held by Tauri.
pub struct AppState {
    pub tx: Sender<EngineCommand>,
    /// The left-pane entries as they stood at startup, for `init`. Kept as entries rather
    /// than finished JSON because the other half of `init` — the authenticated user — can
    /// change after startup, so the payload is rebuilt per call anyway.
    pub init_entries: Vec<LeftPaneEntry>,
    /// The authenticated user, for expanding `@me` and for the sidebar header.
    ///
    /// Shared and mutable because it can arrive late: when the startup lookup can't reach
    /// GitHub the engine keeps retrying and reports over `CurrentUserResolved`, which
    /// `main.rs` writes here. Reading a login captured at startup would keep every `@me`
    /// filter matching nothing for the rest of the session.
    pub current_user: Arc<RwLock<CurrentUserState>>,
    /// DB pool, to rebuild the left pane after structural changes.
    pub pool: SqlitePool,
    /// Whether desktop notifications fire; toggled at runtime via save_settings and read
    /// by the engine-message loop in main.rs through a shared clone.
    pub notifications_enabled: Arc<AtomicBool>,
    /// Root-query id -> display label, for notification text.
    pub query_names: Arc<Mutex<HashMap<i64, String>>>,
    /// User-defined actions from actions.toml, loaded once at startup. JS refers to them
    /// by name; the definitions never cross the IPC boundary.
    pub custom_actions: CustomActions,
}

impl AppState {
    /// The authenticated login as it stands *now*. Read per call, never cached in a
    /// command, so a login that resolved mid-session takes effect immediately.
    ///
    /// Cloned rather than handing out the guard: holding a read lock across the caller's
    /// work would serialize commands for no reason. Poisoning is recovered from — a
    /// panicking writer cannot leave a plain [`CurrentUserState`] half-updated.
    fn login(&self) -> Option<String> {
        self.current_user
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .login
            .clone()
    }

    /// The authenticated user as it stands *now*, cloned for the same reasons as
    /// [`AppState::login`].
    fn user(&self) -> CurrentUserState {
        self.current_user
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// One left-pane entry's unread count: the `(is_filter_stream, entry_id) -> count` map
/// from `compute_unread_counts`, in a shape the front-end can key on directly.
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

/// Return the initial state: the left-pane entries captured at startup, plus the
/// authenticated user *as it stands now*.
///
/// Rebuilt per call rather than served from a snapshot because the login can arrive after
/// startup, and a WebView reload re-runs the front-end's startup against this payload — a
/// snapshot would permanently resurrect "login unknown", since `CurrentUserResolved` has
/// already been sent and won't come again.
///
/// Assembled as an `EngineInit` so serde owns the field names.
#[tauri::command]
pub fn init(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let user = state.user();
    serde_json::to_value(EngineInit {
        entries: state.init_entries.clone(),
        current_user: user.login,
        current_user_name: user.name,
        current_user_avatar_url: user.avatar_url,
    })
    .map_err(|e| e.to_string())
}

/// Quit the app. A command rather than a window close from JS, because the `core:default`
/// capability doesn't include window-close permissions.
#[tauri::command]
pub fn quit(app: tauri::AppHandle) {
    app.exit(0);
}

/// Rebuild the left-pane entries from the DB, called after any structural change. The
/// ordering lives in `glauca_core::engine::load_left_pane_entries`, shared with
/// `Engine::start`, so it is never re-implemented per front-end.
#[tauri::command]
pub async fn list_entries(state: State<'_, AppState>) -> Result<Vec<LeftPaneEntry>, String> {
    let entries = load_left_pane_entries(&state.pool)
        .await
        .map_err(|e| e.to_string())?;
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

/// Persist settings and apply the notifications flag immediately. A sync-interval change
/// takes effect on the next launch, since the engine is already running. Takes the whole
/// [`TauriSettings`] struct so adding a setting touches only settings.rs.
#[tauri::command]
pub fn save_settings(state: State<'_, AppState>, settings: TauriSettings) -> Result<(), String> {
    // Persist first, then flip the in-memory flag, so a failed write leaves the two in
    // agreement rather than diverging for the rest of the session.
    settings.save().map_err(|e| e.to_string())?;
    state
        .notifications_enabled
        .store(settings.notifications_enabled, Ordering::Relaxed);
    Ok(())
}

/// Compute per-entry unread counts for the entries under `query_id` via
/// `glauca_core::logic::compute_unread_counts`. `items` is the front-end's in-memory list,
/// with the `last_read_updated_at` values it advances locally on read.
// This and the next two commands are `async` on purpose: Tauri runs async commands on the
// runtime thread pool and synchronous ones on the UI thread. Their bodies are CPU-bound
// and `filter_items` fires per keystroke, so running them on the UI thread would jank
// input on large queries. They await nothing; `async` alone moves them off that thread.
#[tauri::command]
pub async fn unread_counts(
    state: State<'_, AppState>,
    entries: Vec<LeftPaneEntry>,
    query_id: i64,
    items: Vec<ItemEntry>,
) -> Result<Vec<UnreadCount>, String> {
    let login = state.login();
    Ok(
        compute_unread_counts(&entries, query_id, &items, login.as_deref())
            .into_iter()
            .map(|((is_filter_stream, entry_id), count)| UnreadCount {
                is_filter_stream,
                entry_id,
                count,
            })
            .collect(),
    )
}

/// One piece of an item title, pre-split on the inline filter's match boundaries.
/// `FilterQuery::highlight_ranges` speaks UTF-8 byte offsets, which JS strings cannot index
/// safely, so the conversion happens here rather than crossing the IPC boundary.
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

/// The result of [`filter_items`]: what matched, and whether the answer should be
/// trusted at face value.
///
/// `me_warning` rides along instead of being derived in JS because this command is the one
/// place holding both halves of the question — the filters it just applied and the login it
/// applied them with. It carries core's wording rather than a bool, so the front-end has no
/// copy of the message to drift from.
#[derive(Serialize)]
pub struct FilterResult {
    pub items: Vec<FilteredItem>,
    /// Set when a filter here leans on `@me` while the login is unknown, so it
    /// matched nothing — see [`glauca_core::logic::has_unexpanded_me`].
    pub me_warning: Option<&'static str>,
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

/// Return the entries of `items` matching the selected entry's filter: the filter-stream
/// filter (`None` for a root query) ANDed with the inline search-box text. Indices, not
/// items, are returned so the front-end keeps its own objects and the
/// `last_read_updated_at` values it advances locally on read.
#[tauri::command]
pub async fn filter_items(
    state: State<'_, AppState>,
    items: Vec<ItemEntry>,
    stream_filter: Option<String>,
    inline_filter: String,
) -> Result<FilterResult, String> {
    let login = state.login();
    let su = login.as_deref();
    let stream_q = stream_filter.as_deref().map(|s| StreamFilter::parse(s, su));
    let inline_q = FilterQuery::parse(&expand_me(su, &inline_filter));
    let me_warning = has_unexpanded_me(su, stream_filter.as_deref(), &inline_filter)
        .then_some(ME_UNEXPANDED_WARNING);
    let matched: Vec<FilteredItem> = items
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
        .collect();
    Ok(FilterResult {
        items: matched,
        me_warning,
    })
}

/// What the banner needs to know about a background sync's results: whether to show
/// anything, and what to say.
///
/// Carries the rendered `label` rather than leaving JS to format one, so the wording lives
/// only in `ChangeCounts::banner_label`. `total` is likewise pre-computed so the "anything
/// to show?" test is core's `is_empty`, not a re-derivation in JS.
#[derive(serde::Serialize)]
pub struct ItemChanges {
    pub total: usize,
    pub label: String,
}

/// Diff `fresh` against `current` via `glauca_core::logic::count_changes`. Removals are
/// part of the result, so a sync that only pruned is not mistaken for "nothing changed".
#[tauri::command]
pub async fn count_item_changes(current: Vec<ItemEntry>, fresh: Vec<ItemEntry>) -> ItemChanges {
    let counts: ChangeCounts = count_changes(&current, &fresh);
    ItemChanges {
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
pub async fn reorder_query(
    state: State<'_, AppState>,
    upper_id: i64,
    lower_id: i64,
    active_id: i64,
) -> Result<(), String> {
    dispatch(
        &state.tx,
        EngineCommand::ReorderQuery {
            upper_id,
            lower_id,
            active_id,
        },
    )
    .await
}

#[tauri::command]
pub async fn reorder_filter_stream(
    state: State<'_, AppState>,
    upper_id: i64,
    lower_id: i64,
    active_id: i64,
) -> Result<(), String> {
    dispatch(
        &state.tx,
        EngineCommand::ReorderFilterStream {
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

/// A custom action as exposed to the front-end: just enough to render a picker. The
/// command template and env stay on the Rust side, so user-defined command lines never
/// enter the webview — JS runs an action by `name` via [`run_custom_action`].
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
    // The engine expects an already-`@me`-expanded filter, so the front-end can pass the
    // raw filter-stream filter.
    let login = state.login();
    let filter = filter.map(|f| expand_me(login.as_deref(), &f).into_owned());
    dispatch(&state.tx, EngineCommand::MarkAllRead { query_id, filter }).await
}
