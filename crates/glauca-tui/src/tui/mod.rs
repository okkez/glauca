use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use octocrab::Octocrab;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use sqlx::SqlitePool;
use std::{
    cell::RefCell,
    collections::HashMap,
    io,
    time::{SystemTime, UNIX_EPOCH},
};

pub mod icons;
mod keys;
mod message;
mod process;
mod select;
pub mod settings;
pub mod single_line_input;
mod state;
pub mod ui;

#[cfg(test)]
mod test_support;

use icons::Icons;
use keys::handle_key;
use message::handle_app_message;
pub(crate) use process::{
    copy_to_clipboard_osc52, item_actions, restore_tui, run_editor, run_octorus_review, suspend_tui,
};
use select::{
    full_resync_selected, mark_selected_item_read, refresh_selected_item, refresh_selected_list,
    reorder_command, select_current_entry,
};
use single_line_input::SingleLineInput;
pub(crate) use state::{
    clear_active_modal_field, modal_fields, modal_fields_ref, sync_modal_cursors,
};

use glauca_core::actions::{CustomAction, CustomActions};
use glauca_core::engine::{AppMessage, Engine, EngineCommand, ReviewEvent};
use glauca_core::filter::FilterQuery;
use glauca_core::logic::{group_range, is_item_unread, move_group_down, query_label};
use glauca_core::notify::ItemTracker;
use settings::TuiSettings;

// ── Display/domain types ─────────────────────────────────────────────────────
// Moved to glauca-core::types (framework 非依存)。TUI からは従来名で使えるよう re-export。
pub use glauca_core::types::{
    CommentEntry, ItemAction, ItemEntry, LeftPaneEntry, MergeStrategy, QueryEntry,
};

// ── Application state ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Focus {
    QueryList,
    ItemList,
    ItemDetail,
}

#[derive(Debug, PartialEq)]
pub enum InputMode {
    Normal,
    Filter,
    NewQuery,
    /// New filter stream modal (name + filter, Tab to switch fields).
    NewFilterStream,
    /// Edit root query modal (display name + GitHub search query, Tab to switch fields).
    EditQuery,
    /// Edit filter stream modal (name + filter, Tab to switch fields).
    EditFilterStream,
    ActionMenu,
    /// User-defined custom-action picker (opened with `x`).
    CustomActionMenu,
    MergeMenu,
    /// Review-event selection (Comment / Approve / Request changes), shown after
    /// the review-comment editor closes so the submit can be confirmed/cancelled.
    ReviewMenu,
    /// Comments popup (fetched via API, displayed in-TUI).
    CommentsPopup,
    /// Keybinding cheat-sheet overlay (opened with `?`).
    Help,
}

/// Memoized result of `filter_items` for the current list. `filtered_items()`
/// is called several times per render (list, detail, status bar) and each call
/// otherwise re-parses the query and fuzzy-matches every item; caching the
/// matching indices collapses those to one pass while the inputs are unchanged.
/// Stores indices (not `&ItemEntry`) so it doesn't borrow `App::items`.
#[derive(Default)]
struct FilteredCache {
    /// `(items_version, stream_filter, inline_filter, current_user)` the cached
    /// `indices` were computed from. A mismatch triggers recomputation.
    key: Option<(u64, Option<String>, String, Option<String>)>,
    indices: Vec<usize>,
}

pub struct App {
    pub focus: Focus,
    pub input_mode: InputMode,

    pub entries: Vec<LeftPaneEntry>,
    pub entry_cursor: usize,

    /// The visible item list. Change it structurally only through
    /// `apply_items_to_view` / `clear_items`, which bump `items_version` to
    /// invalidate `filtered_cache`; a direct replace/reorder here would leave the
    /// memoized filter indices stale (in-place field edits like marking-read are
    /// fine, since they don't affect which items match).
    pub items: Vec<ItemEntry>,
    /// Bumped whenever `items` is replaced, to invalidate `filtered_cache`.
    items_version: u64,
    /// Memoized `filter_items` indices; see [`FilteredCache`].
    filtered_cache: RefCell<FilteredCache>,
    pub item_cursor: usize,
    pub unread_counts: HashMap<(bool, i64), usize>,
    /// Freshly-synced items for the currently-viewed query, held back because
    /// they came from a background sync. Applied on explicit action (`u`).
    pub pending_items: Option<Vec<ItemEntry>>,
    /// How many of `pending_items` are new/updated vs the displayed list.
    pub pending_count: usize,
    pub filter: SingleLineInput,
    /// Active filter stream filter applied before the inline filter (if any).
    pub stream_filter: Option<String>,

    pub new_query_input: SingleLineInput,
    pub new_query_name: SingleLineInput,
    pub new_filter_stream_name: SingleLineInput,
    pub new_filter_stream_filter: SingleLineInput,
    /// Input buffer reused for edit modals (display name or step-1 field).
    pub edit_input: SingleLineInput,
    /// Second input buffer for edit modals (query string or filter string).
    pub edit_input2: SingleLineInput,
    /// Which field (0 or 1) is active in a 2-field modal.
    pub modal_field: usize,
    pub action_cursor: usize,
    pub merge_strategy_cursor: usize,
    /// Selection cursor for the review-event menu (`ReviewEvent::all()` order).
    pub review_event_cursor: usize,
    /// Review comment captured from the editor, pending the review-event choice.
    pub review_body: Option<String>,
    /// Comments fetched for the comments popup.
    pub comments: Vec<CommentEntry>,
    /// True while comments are being fetched from the API.
    pub comments_loading: bool,
    /// Scroll offset within the comments popup.
    pub comments_scroll: usize,
    /// Whether to show minimized/hidden comments (default: false = collapsed).
    pub comments_show_hidden: bool,
    /// Sort order for comments: true = newest first, false = oldest first.
    pub comments_sort_desc: bool,
    pub status: Option<String>,
    /// Whether a manual GitHub sync is in progress for the selected query.
    pub syncing: bool,
    /// Number of pending background auto-refresh jobs (queued + in-progress).
    pub bg_sync_pending: usize,
    /// Scroll offset for the detail pane (right column).
    pub detail_scroll: u16,
    /// Login name of the authenticated GitHub user (used to expand `@me` in filters).
    pub current_user: Option<String>,
    /// Whether background-sync arrivals fire OS desktop notifications. Loaded
    /// from `TuiSettings`; toggled with `N`.
    pub notifications_enabled: bool,
    /// Per-query session baseline for the notification "N updated" count, so the
    /// first load of each query establishes a baseline without notifying (no
    /// startup storm). See `glauca_core::notify::ItemTracker`.
    pub notif_tracker: ItemTracker,
    /// Active semantic-icon set (emoji/Unicode vs icon-font glyphs). Loaded from
    /// `TuiSettings::use_icon_font`; toggled with `F`.
    pub icons: Icons,
    /// User-defined custom actions loaded from `actions.toml` (see
    /// `glauca_core::actions`). Offered via the picker (`x`), filtered by kind.
    pub custom_actions: CustomActions,
    /// Selection cursor within the custom-action picker (indexes the list
    /// returned by `custom_actions_for_selected`).
    pub custom_action_cursor: usize,
}

// `AppMessage` / `SyncJob` は glauca_core::engine へ移設（A6）。

// ── Actions returned from key handling ───────────────────────────────────────

enum Action {
    None,
    Quit,
    LoadEntry,
    SaveNewQuery,
    SaveNewFilterStream,
    SaveEditQuery,
    SaveEditFilterStream,
    Confirm,
    ConfirmMergeStrategy,
    ConfirmReviewEvent,
    OpenBrowser,
    CopyUrl,
    /// Confirm the highlighted entry in the custom-action picker.
    ConfirmCustom,
    ReviewOctorus,
    RefreshList,
    RefreshItem,
    /// Force a full re-fetch + prune of the selected query (`S`).
    FullResync,
    /// Apply held-back background-sync results to the visible list (`u`).
    ApplyPending,
}

// ── Main run loop ─────────────────────────────────────────────────────────────

pub async fn run(pool: SqlitePool, gh: Octocrab) -> Result<()> {
    // Set up terminal
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, pool, gh).await;

    // Restore terminal unconditionally
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen
    )?;

    result
}

async fn run_app<B: ratatui::backend::Backend + io::Write>(
    terminal: &mut Terminal<B>,
    pool: SqlitePool,
    gh: Octocrab,
) -> Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    // Start the async engine: builds the left-pane entries, resolves the current
    // user, and spawns the background worker / refresh timer / command loop.
    let tui_settings = TuiSettings::load();
    let (mut engine, init) = Engine::start(pool, gh, tui_settings.sync_interval_secs).await?;

    // Build App from the engine's initial entries (filter streams interleaved).
    let queries: Vec<QueryEntry> = init
        .entries
        .iter()
        .filter_map(|e| match e {
            LeftPaneEntry::Query(q) => Some(q.clone()),
            LeftPaneEntry::FilterStream(_) => None,
        })
        .collect();
    let mut app = App::new(queries);
    app.entries = init.entries;
    app.current_user = init.current_user;
    app.notifications_enabled = tui_settings.notifications_enabled;
    app.icons = Icons::new(tui_settings.use_icon_font);
    app.custom_actions = CustomActions::load();

    // Prime unread counts for every root query via a cached load (no sync).
    let root_query_ids: Vec<i64> = app
        .entries
        .iter()
        .filter_map(|entry| match entry {
            LeftPaneEntry::Query(q) => Some(q.id),
            LeftPaneEntry::FilterStream(_) => None,
        })
        .collect();
    for query_id in &root_query_ids {
        engine
            .send(EngineCommand::LoadCached {
                query_id: *query_id,
            })
            .await;
    }

    // Load items for the initially selected entry; sync only if the cache is stale.
    let initially_synced_id = select_current_entry(&mut app, &engine, false).await;

    // Enqueue all other stale queries for immediate background refresh.
    engine
        .send(EngineCommand::EnqueueStale {
            skip_query_id: initially_synced_id,
        })
        .await;

    let mut events = EventStream::new();

    loop {
        terminal.draw(|f| ui::draw(f, &app))?;

        tokio::select! {
            Some(Ok(event)) = events.next() => {
                if let Event::Key(key) = event {
                    // Ignore key-release events. Terminals with the keyboard-
                    // enhancement protocol (or Windows) emit them, and acting on
                    // both press and release would double-fire actions like
                    // 'd'/'J'/'K'. Repeat events are kept so held-key repeat still
                    // works if enhancement flags are ever enabled.
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    // 'd' in query list → delete selected entry (UI updates on the
                    // QueryDeleted / FilterStreamDeleted message once the DB op succeeds).
                    if key.code == KeyCode::Char('d')
                        && app.focus == Focus::QueryList
                        && app.input_mode == InputMode::Normal
                    {
                        let cmd = match app.entries.get(app.entry_cursor) {
                            Some(LeftPaneEntry::Query(q)) => {
                                Some(EngineCommand::DeleteQuery { query_id: q.id })
                            }
                            Some(LeftPaneEntry::FilterStream(fs)) => {
                                Some(EngineCommand::DeleteFilterStream { id: fs.id })
                            }
                            None => None,
                        };
                        if let Some(cmd) = cmd {
                            engine.send(cmd).await;
                        }
                        continue;
                    }

                    // 'a' in query list → mark all items of the selected entry read.
                    // A query marks its whole root query; a filter stream marks only
                    // its matching items (filter expanded with the current user here,
                    // since the engine does not know `@me`). The engine persists and
                    // reloads the query, which refreshes unread counts via ItemsLoaded.
                    if key.code == KeyCode::Char('a')
                        && app.focus == Focus::QueryList
                        && app.input_mode == InputMode::Normal
                    {
                        let cmd = app.entries.get(app.entry_cursor).map(|entry| {
                            EngineCommand::MarkAllRead {
                                query_id: entry.root_query_id(),
                                filter: entry.stream_filter().map(|f| {
                                    glauca_core::logic::expand_me(app.current_user.as_deref(), f)
                                        .into_owned()
                                }),
                            }
                        });
                        if let Some(cmd) = cmd {
                            engine.send(cmd).await;
                        }
                        continue;
                    }

                    // J/K: move selected entry up/down within its group. The DB swap
                    // runs through the engine; the entries vec is reordered on the
                    // QueriesSwapped / FilterStreamsSwapped confirmation message.
                    if (key.code == KeyCode::Char('J') || key.code == KeyCode::Char('K'))
                        && app.focus == Focus::QueryList
                        && app.input_mode == InputMode::Normal
                    {
                        let down = key.code == KeyCode::Char('J');
                        if let Some(cmd) = reorder_command(&app.entries, app.entry_cursor, down) {
                            engine.send(cmd).await;
                        }
                        continue;
                    }

                    let action = handle_key(&mut app, key);
                    // Keep the visible text cursor on the active modal field.
                    sync_modal_cursors(&mut app);
                    match action {
                        Action::Quit => break,
                        Action::LoadEntry => {
                            app.filter = SingleLineInput::new();
                            app.item_cursor = 0;
                            app.detail_scroll = 0;
                            app.clear_items();
                            select_current_entry(&mut app, &engine, true).await;
                        }
                        Action::SaveNewQuery => {
                            let query_str = app.new_query_input.value().trim().to_string();
                            let name_str = app.new_query_name.value().trim().to_string();
                            app.input_mode = InputMode::Normal;
                            app.modal_field = 0;
                            app.new_query_input = SingleLineInput::new();
                            app.new_query_name = SingleLineInput::new();
                            let name = if name_str.is_empty() {
                                None
                            } else {
                                Some(name_str)
                            };
                            engine
                                .send(EngineCommand::AddQuery {
                                    name,
                                    query: query_str,
                                })
                                .await;
                        }
                        Action::SaveNewFilterStream => {
                            let name = app.new_filter_stream_name.value().trim().to_string();
                            let filter =
                                app.new_filter_stream_filter.value().trim().to_string();
                            app.input_mode = InputMode::Normal;
                            app.new_filter_stream_name = SingleLineInput::new();
                            app.new_filter_stream_filter = SingleLineInput::new();

                            // Determine parent: root_query_id of the currently selected entry
                            if let Some(entry) = app.entries.get(app.entry_cursor) {
                                let parent_id = entry.root_query_id();
                                let kind = entry.kind().to_string();
                                engine
                                    .send(EngineCommand::AddFilterStream {
                                        parent_id,
                                        kind,
                                        name,
                                        filter,
                                    })
                                    .await;
                            }
                        }
                        Action::SaveEditFilterStream => {
                            let name = app.edit_input.value().trim().to_string();
                            let filter = app.edit_input2.value().trim().to_string();
                            app.input_mode = InputMode::Normal;
                            app.edit_input = SingleLineInput::new();
                            app.edit_input2 = SingleLineInput::new();

                            if let Some(LeftPaneEntry::FilterStream(fs)) =
                                app.entries.get(app.entry_cursor)
                            {
                                engine
                                    .send(EngineCommand::EditFilterStream {
                                        id: fs.id,
                                        name,
                                        filter,
                                    })
                                    .await;
                            }
                        }
                        Action::SaveEditQuery => {
                            let name_input = app.edit_input.value().trim().to_string();
                            let new_query = app.edit_input2.value().trim().to_string();
                            app.input_mode = InputMode::Normal;
                            app.edit_input = SingleLineInput::new();
                            app.edit_input2 = SingleLineInput::new();

                            if let Some(LeftPaneEntry::Query(q)) =
                                app.entries.get(app.entry_cursor)
                            {
                                // Empty name means "use query string as label"
                                let new_name: Option<String> = if name_input.is_empty() {
                                    None
                                } else {
                                    Some(name_input)
                                };
                                engine
                                    .send(EngineCommand::EditQuery {
                                        id: q.id,
                                        name: new_name,
                                        query: new_query,
                                    })
                                    .await;
                            }
                        }
                        Action::Confirm => {
                            if let Some(item) = app.selected_item().cloned() {
                                let actions = item_actions(&item.kind);
                                if let Some(action) = actions.get(app.action_cursor).cloned() {
                                    match action {
                                        ItemAction::OpenBrowser => {
                                            app.input_mode = InputMode::Normal;
                                            engine
                                                .send(EngineCommand::OpenBrowser {
                                                    item: Box::new(item.clone()),
                                                })
                                                .await;
                                        }
                                        ItemAction::Comment => {
                                            app.input_mode = InputMode::Normal;
                                            suspend_tui(terminal)?;
                                            let editor_result = run_editor("");
                                            restore_tui(terminal)?;

                                            match editor_result {
                                                Ok(Some(body)) => {
                                                    engine
                                                        .send(EngineCommand::Comment {
                                                            url: item.url.clone(),
                                                            kind: item.kind.clone(),
                                                            body,
                                                        })
                                                        .await;
                                                }
                                                Ok(None) => {
                                                    app.status = Some("Comment cancelled".into());
                                                }
                                                Err(e) => {
                                                    app.status = Some(format!("Editor error: {e}"));
                                                }
                                            }
                                        }
                                        ItemAction::ViewComments => {
                                            // Open in-TUI comments popup: fetch via API in background
                                            app.input_mode = InputMode::CommentsPopup;
                                            app.comments.clear();
                                            app.comments_loading = true;
                                            app.comments_scroll = 0;
                                            engine
                                                .send(EngineCommand::LoadComments {
                                                    owner: item.repo_owner.clone(),
                                                    repo: item.repo_name.clone(),
                                                    number: item.number as u64,
                                                })
                                                .await;
                                        }
                                        ItemAction::ApprovePR => {
                                            app.input_mode = InputMode::Normal;
                                            suspend_tui(terminal)?;
                                            let editor_result = run_editor(
                                                "# Review comment (required for Comment / Request changes; optional for Approve)\n# Lines starting with '#' are ignored.\n",
                                            );
                                            restore_tui(terminal)?;

                                            match editor_result {
                                                Ok(body_opt) => {
                                                    // Strip comment lines; empty → no body. Then
                                                    // confirm the review event before submitting.
                                                    app.review_body = body_opt.and_then(|body| {
                                                        let stripped = body
                                                            .lines()
                                                            .filter(|line| !line.starts_with('#'))
                                                            .collect::<Vec<_>>()
                                                            .join("\n");
                                                        let stripped = stripped.trim().to_string();
                                                        (!stripped.is_empty()).then_some(stripped)
                                                    });
                                                    app.review_event_cursor = 0;
                                                    app.input_mode = InputMode::ReviewMenu;
                                                }
                                                Err(e) => {
                                                    app.status = Some(format!("Editor error: {e}"));
                                                }
                                            }
                                        }
                                        ItemAction::MergePR => {
                                            app.input_mode = InputMode::MergeMenu;
                                            app.merge_strategy_cursor = 0;
                                        }
                                        ItemAction::CopyUrl => {
                                            app.input_mode = InputMode::Normal;
                                            app.status = Some(match copy_to_clipboard_osc52(&item.url) {
                                                Ok(()) => "Copied URL to clipboard".into(),
                                                Err(e) => format!("Copy failed: {e}"),
                                            });
                                        }
                                        ItemAction::ReviewOctorus => {
                                            app.input_mode = InputMode::Normal;
                                            app.status = Some(run_octorus_review(terminal, &item)?);
                                        }
                                        ItemAction::RefreshItem => {
                                            app.input_mode = InputMode::Normal;
                                            refresh_selected_item(&mut app, &engine).await;
                                        }
                                    }
                                }
                            }
                        }
                        Action::ConfirmMergeStrategy => {
                            if let Some(item) = app.selected_item().cloned() {
                                let strategies = MergeStrategy::all();
                                if let Some(strategy) = strategies.get(app.merge_strategy_cursor).cloned() {
                                    app.input_mode = InputMode::Normal;
                                    engine
                                        .send(EngineCommand::Merge {
                                            url: item.url.clone(),
                                            strategy,
                                        })
                                        .await;
                                }
                            }
                        }
                        Action::ConfirmReviewEvent => {
                            if let Some(item) = app.selected_item().cloned()
                                && let Some(event) =
                                    ReviewEvent::all().get(app.review_event_cursor).copied()
                            {
                                // gh requires a body for comment / request-changes.
                                if event.requires_body() && app.review_body.is_none() {
                                    app.status = Some(
                                        "Review comment required for Comment / Request changes"
                                            .into(),
                                    );
                                } else {
                                    app.input_mode = InputMode::Normal;
                                    let body = app.review_body.take();
                                    engine
                                        .send(EngineCommand::SubmitReview {
                                            url: item.url.clone(),
                                            event,
                                            body,
                                        })
                                        .await;
                                }
                            }
                        }
                        Action::OpenBrowser => {
                            if let Some(item) = app.selected_item().cloned() {
                                engine
                                    .send(EngineCommand::OpenBrowser {
                                        item: Box::new(item),
                                    })
                                    .await;
                            }
                        }
                        Action::CopyUrl => {
                            if let Some(item) = app.selected_item().cloned() {
                                app.status = Some(match copy_to_clipboard_osc52(&item.url) {
                                    Ok(()) => "Copied URL to clipboard".into(),
                                    Err(e) => format!("Copy failed: {e}"),
                                });
                            }
                        }
                        Action::ConfirmCustom => {
                            let action = app
                                .custom_actions_for_selected()
                                .get(app.custom_action_cursor)
                                .map(|&a| a.clone());
                            if let (Some(action), Some(item)) =
                                (action, app.selected_item().cloned())
                            {
                                app.input_mode = InputMode::Normal;
                                engine
                                    .send(EngineCommand::RunCustomAction {
                                        action: Box::new(action),
                                        item: Box::new(item),
                                    })
                                    .await;
                            }
                        }
                        Action::ReviewOctorus => {
                            if let Some(item) = app.selected_item().cloned() {
                                app.status = Some(run_octorus_review(terminal, &item)?);
                            }
                        }
                        Action::RefreshList => {
                            refresh_selected_list(&mut app, &engine).await;
                        }
                        Action::RefreshItem => {
                            refresh_selected_item(&mut app, &engine).await;
                        }
                        Action::FullResync => {
                            full_resync_selected(&mut app, &engine).await;
                        }
                        Action::ApplyPending => {
                            app.apply_pending_items();
                        }
                        Action::None => {}
                    }
                    // Viewing an item (cursor on the item list or its detail pane)
                    // marks it read and decrements the unread badge. Idempotent —
                    // a no-op once the item is already read.
                    if app.input_mode == InputMode::Normal
                        && matches!(app.focus, Focus::ItemList | Focus::ItemDetail)
                    {
                        mark_selected_item_read(&mut app, &engine).await;
                    }
                }
            }
            Some(msg) = engine.recv() => {
                handle_app_message(&mut app, &engine, msg).await;
            }
        }
    }

    Ok(())
}
