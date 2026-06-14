use crate::{db, github};
use anyhow::Result;
use chrono::Utc;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use octocrab::Octocrab;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use sqlx::SqlitePool;
use std::{
    collections::HashMap,
    io,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;

pub mod ui;

use glauca_core::engine::{
    AppMessage, CACHE_STALE_SECS, SyncJob, execute_approve, execute_comment, execute_merge,
    execute_open_browser, fetch_comments_task, load_items_task, refresh_timer_task,
    spawn_mark_entry_viewed, sync_task, sync_worker_task,
};
use glauca_core::filter::FilterQuery;
use glauca_core::logic::{cached_item_to_item_entry, group_range, is_item_new_since, move_group_down};

// ── Display/domain types ─────────────────────────────────────────────────────
// Moved to glauca-core::types (framework 非依存)。TUI からは従来名で使えるよう re-export。
pub use glauca_core::types::{
    CommentEntry, FilterStreamEntry, ItemAction, ItemEntry, LeftPaneEntry, MergeStrategy,
    QueryEntry,
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
    MergeMenu,
    /// Comments popup (fetched via API, displayed in-TUI).
    CommentsPopup,
}

pub struct App {
    pub focus: Focus,
    pub input_mode: InputMode,

    pub entries: Vec<LeftPaneEntry>,
    pub entry_cursor: usize,

    pub items: Vec<ItemEntry>,
    pub item_cursor: usize,
    pub unread_counts: HashMap<i64, usize>,
    pub active_entry_last_viewed_at: Option<String>,
    pub filter: String,
    /// Active filter stream filter applied before the inline filter (if any).
    pub stream_filter: Option<String>,

    pub new_query_input: String,
    pub new_query_name: String,
    pub new_filter_stream_name: String,
    pub new_filter_stream_filter: String,
    /// Input buffer reused for edit modals (display name or step-1 field).
    pub edit_input: String,
    /// Second input buffer for edit modals (query string or filter string).
    pub edit_input2: String,
    /// Which field (0 or 1) is active in a 2-field modal.
    pub modal_field: usize,
    pub action_cursor: usize,
    pub merge_strategy_cursor: usize,
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
}

impl App {
    pub fn new(queries: Vec<QueryEntry>) -> Self {
        let entries = queries.into_iter().map(LeftPaneEntry::Query).collect();
        Self {
            focus: Focus::QueryList,
            input_mode: InputMode::Normal,
            entries,
            entry_cursor: 0,
            items: Vec::new(),
            item_cursor: 0,
            unread_counts: HashMap::new(),
            active_entry_last_viewed_at: None,
            filter: String::new(),
            stream_filter: None,
            new_query_input: String::new(),
            new_query_name: String::new(),
            new_filter_stream_name: String::new(),
            new_filter_stream_filter: String::new(),
            edit_input: String::new(),
            edit_input2: String::new(),
            modal_field: 0,
            action_cursor: 0,
            merge_strategy_cursor: 0,
            comments: Vec::new(),
            comments_loading: false,
            comments_scroll: 0,
            comments_show_hidden: false,
            comments_sort_desc: false,
            status: None,
            syncing: false,
            bg_sync_pending: 0,
            detail_scroll: 0,
            current_user: None,
        }
    }

    pub fn parsed_filter(&self) -> FilterQuery {
        FilterQuery::parse(&self.expand_me(&self.filter))
    }

    /// Replace `@me` with the authenticated user's login (case-insensitive).
    /// Falls back to `@me` unchanged if the user is not known yet.
    fn expand_me<'a>(&'a self, s: &'a str) -> std::borrow::Cow<'a, str> {
        glauca_core::logic::expand_me(self.current_user.as_deref(), s)
    }

    pub fn filtered_items(&self) -> Vec<&ItemEntry> {
        glauca_core::logic::filter_items(
            &self.items,
            self.stream_filter.as_deref(),
            &self.filter,
            self.current_user.as_deref(),
        )
    }

    pub fn selected_item(&self) -> Option<&ItemEntry> {
        let filtered = self.filtered_items();
        filtered.get(self.item_cursor).copied()
    }

    pub fn selected_entry(&self) -> Option<&LeftPaneEntry> {
        self.entries.get(self.entry_cursor)
    }

    /// Returns the root query id for the currently selected entry.
    pub fn selected_root_query_id(&self) -> Option<i64> {
        self.selected_entry().map(|e| e.root_query_id())
    }

    fn clamp_item_cursor(&mut self) {
        let max = self.filtered_items().len().saturating_sub(1);
        if self.item_cursor > max {
            self.item_cursor = max;
        }
    }

    fn mark_entry_viewed(&mut self, entry_id: i64, viewed_at: String) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id() == entry_id) {
            entry.set_last_viewed_at(Some(viewed_at));
        }
    }

    fn recompute_unread_counts_for_query(&mut self, query_id: i64, items: &[ItemEntry]) {
        for (entry_id, unread) in glauca_core::logic::compute_unread_counts(
            &self.entries,
            query_id,
            items,
            self.current_user.as_deref(),
        ) {
            self.unread_counts.insert(entry_id, unread);
        }
    }
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
    ConfirmAction,
    ConfirmMergeStrategy,
    OpenBrowser,
}

// ── Key event handler ────────────────────────────────────────────────────────

fn handle_key(app: &mut App, key: KeyEvent) -> Action {
    match app.input_mode {
        InputMode::Filter => handle_key_filter(app, key),
        InputMode::NewQuery => handle_key_new_query(app, key),
        InputMode::NewFilterStream => handle_key_new_filter_stream(app, key),
        InputMode::EditQuery => handle_key_edit_query(app, key),
        InputMode::EditFilterStream => handle_key_edit_filter_stream(app, key),
        InputMode::ActionMenu => handle_key_action_menu(app, key),
        InputMode::MergeMenu => handle_key_merge_menu(app, key),
        InputMode::CommentsPopup => handle_key_comments_popup(app, key),
        InputMode::Normal => handle_key_normal(app, key),
    }
}

fn handle_key_normal(app: &mut App, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('q') => return Action::Quit,

        // Focus cycling — h/l or left/right arrows
        KeyCode::Char('l') | KeyCode::Right => {
            app.focus = match app.focus {
                Focus::QueryList => Focus::ItemList,
                Focus::ItemList => Focus::ItemDetail,
                Focus::ItemDetail => Focus::QueryList,
            };
        }
        KeyCode::Char('h') | KeyCode::Left => {
            app.focus = match app.focus {
                Focus::QueryList => Focus::ItemDetail,
                Focus::ItemList => Focus::QueryList,
                Focus::ItemDetail => Focus::ItemList,
            };
        }

        // Navigation
        KeyCode::Char('j') | KeyCode::Down => match app.focus {
            Focus::QueryList => {
                if app.entry_cursor + 1 < app.entries.len() {
                    app.entry_cursor += 1;
                    return Action::LoadEntry;
                }
            }
            Focus::ItemList => {
                let max = app.filtered_items().len().saturating_sub(1);
                if app.item_cursor < max {
                    app.item_cursor += 1;
                    app.detail_scroll = 0;
                }
            }
            Focus::ItemDetail => {
                app.detail_scroll = app.detail_scroll.saturating_add(1);
            }
        },
        KeyCode::Char('k') | KeyCode::Up => match app.focus {
            Focus::QueryList => {
                if app.entry_cursor > 0 {
                    app.entry_cursor -= 1;
                    return Action::LoadEntry;
                }
            }
            Focus::ItemList => {
                if app.item_cursor > 0 {
                    app.item_cursor -= 1;
                    app.detail_scroll = 0;
                }
            }
            Focus::ItemDetail => {
                app.detail_scroll = app.detail_scroll.saturating_sub(1);
            }
        },

        // New root query (left pane)
        KeyCode::Char('n') if app.focus == Focus::QueryList => {
            app.input_mode = InputMode::NewQuery;
            app.modal_field = 0;
            app.new_query_name.clear();
            app.new_query_input.clear();
        }
        // New filter stream (left pane) — only when a root query or filter stream is selected
        KeyCode::Char('f') if app.focus == Focus::QueryList => {
            if !app.entries.is_empty() {
                app.input_mode = InputMode::NewFilterStream;
                app.modal_field = 0;
                app.new_filter_stream_name.clear();
                app.new_filter_stream_filter.clear();
            }
        }
        // Edit selected entry (left pane)
        KeyCode::Char('e') if app.focus == Focus::QueryList => {
            if let Some(entry) = app.entries.get(app.entry_cursor) {
                match entry {
                    LeftPaneEntry::Query(q) => {
                        app.edit_input = q.label.clone();
                        app.edit_input2 = q.query_str.clone();
                        app.modal_field = 0;
                        app.input_mode = InputMode::EditQuery;
                    }
                    LeftPaneEntry::FilterStream(fs) => {
                        app.edit_input = fs.name.clone();
                        app.edit_input2 = fs.filter.clone();
                        app.modal_field = 0;
                        app.input_mode = InputMode::EditFilterStream;
                    }
                }
            }
        }
        // Delete handled in main loop
        KeyCode::Char('d')
            if app.focus == Focus::QueryList && key.modifiers.contains(KeyModifiers::NONE) => {}

        KeyCode::Enter
            if matches!(app.focus, Focus::ItemList | Focus::ItemDetail)
                && app.selected_item().is_some() =>
        {
            app.input_mode = InputMode::ActionMenu;
            app.action_cursor = 0;
        }

        // Open selected item in browser directly
        KeyCode::Char('o')
            if matches!(app.focus, Focus::ItemList | Focus::ItemDetail)
                && app.selected_item().is_some() =>
        {
            return Action::OpenBrowser;
        }

        // Enter filter mode (middle pane)
        KeyCode::Char('/') if app.focus == Focus::ItemList => {
            app.input_mode = InputMode::Filter;
        }

        _ => {}
    }
    Action::None
}

fn handle_key_action_menu(app: &mut App, key: KeyEvent) -> Action {
    let available_len = app
        .selected_item()
        .map(|item| ItemAction::available_for(&item.kind).len())
        .unwrap_or(0);

    match key.code {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            let max = available_len.saturating_sub(1);
            if app.action_cursor < max {
                app.action_cursor += 1;
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.action_cursor = app.action_cursor.saturating_sub(1);
        }
        KeyCode::Enter => return Action::ConfirmAction,
        _ => {}
    }

    Action::None
}

fn handle_key_merge_menu(app: &mut App, key: KeyEvent) -> Action {
    let max = MergeStrategy::all().len().saturating_sub(1);

    match key.code {
        KeyCode::Esc => {
            app.input_mode = InputMode::ActionMenu;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if app.merge_strategy_cursor < max {
                app.merge_strategy_cursor += 1;
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.merge_strategy_cursor = app.merge_strategy_cursor.saturating_sub(1);
        }
        KeyCode::Enter => return Action::ConfirmMergeStrategy,
        _ => {}
    }

    Action::None
}

fn handle_key_comments_popup(app: &mut App, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.input_mode = InputMode::Normal;
            app.comments.clear();
            app.comments_loading = false;
            app.comments_scroll = 0;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            app.comments_scroll = app.comments_scroll.saturating_add(1);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.comments_scroll = app.comments_scroll.saturating_sub(1);
        }
        KeyCode::Char('g') => {
            app.comments_scroll = 0;
        }
        KeyCode::Char('G') => {
            app.comments_scroll = app.comments_scroll.saturating_add(9999);
        }
        KeyCode::Char('h') => {
            app.comments_show_hidden = !app.comments_show_hidden;
            app.comments_scroll = 0;
        }
        KeyCode::Char('s') => {
            app.comments_sort_desc = !app.comments_sort_desc;
            app.comments_scroll = 0;
        }
        _ => {}
    }
    Action::None
}

fn handle_key_filter(app: &mut App, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace => {
            app.filter.pop();
            app.item_cursor = 0;
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.filter.clear();
            app.item_cursor = 0;
        }
        KeyCode::Char(c) => {
            app.filter.push(c);
            app.item_cursor = 0;
        }
        _ => {}
    }
    Action::None
}

fn handle_key_new_query(app: &mut App, key: KeyEvent) -> Action {
    // field 0 = display name (optional), field 1 = GitHub search query
    match key.code {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
            app.new_query_name.clear();
            app.new_query_input.clear();
            app.modal_field = 0;
        }
        KeyCode::Tab => {
            app.modal_field = 1 - app.modal_field;
        }
        KeyCode::Enter => {
            if app.modal_field == 0 {
                // Move focus to the query field
                app.modal_field = 1;
            } else if !app.new_query_input.trim().is_empty() {
                return Action::SaveNewQuery;
            }
        }
        KeyCode::Backspace => {
            if app.modal_field == 0 {
                app.new_query_name.pop();
            } else {
                app.new_query_input.pop();
            }
        }
        KeyCode::Char(c) => {
            if app.modal_field == 0 {
                app.new_query_name.push(c);
            } else {
                app.new_query_input.push(c);
            }
        }
        _ => {}
    }
    Action::None
}

fn handle_key_new_filter_stream(app: &mut App, key: KeyEvent) -> Action {
    // field 0 = name, field 1 = filter
    match key.code {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
            app.new_filter_stream_name.clear();
            app.new_filter_stream_filter.clear();
        }
        KeyCode::Tab => {
            app.modal_field = 1 - app.modal_field;
        }
        KeyCode::Enter => {
            if !app.new_filter_stream_name.trim().is_empty()
                && !app.new_filter_stream_filter.trim().is_empty()
            {
                return Action::SaveNewFilterStream;
            }
            // Move to the empty field if one is missing
            if app.new_filter_stream_name.trim().is_empty() {
                app.modal_field = 0;
            } else {
                app.modal_field = 1;
            }
        }
        KeyCode::Backspace => {
            if app.modal_field == 0 {
                app.new_filter_stream_name.pop();
            } else {
                app.new_filter_stream_filter.pop();
            }
        }
        KeyCode::Char(c) => {
            if app.modal_field == 0 {
                app.new_filter_stream_name.push(c);
            } else {
                app.new_filter_stream_filter.push(c);
            }
        }
        _ => {}
    }
    Action::None
}

fn handle_key_edit_query(app: &mut App, key: KeyEvent) -> Action {
    // field 0 = display name, field 1 = GitHub search query
    match key.code {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
            app.edit_input.clear();
            app.edit_input2.clear();
        }
        KeyCode::Tab => {
            app.modal_field = 1 - app.modal_field;
        }
        KeyCode::Enter => {
            if !app.edit_input2.trim().is_empty() {
                return Action::SaveEditQuery;
            }
            app.modal_field = 1; // move focus to the query field
        }
        KeyCode::Backspace => {
            if app.modal_field == 0 {
                app.edit_input.pop();
            } else {
                app.edit_input2.pop();
            }
        }
        KeyCode::Char(c) => {
            if app.modal_field == 0 {
                app.edit_input.push(c);
            } else {
                app.edit_input2.push(c);
            }
        }
        _ => {}
    }
    Action::None
}

fn handle_key_edit_filter_stream(app: &mut App, key: KeyEvent) -> Action {
    // field 0 = name, field 1 = filter
    match key.code {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
            app.edit_input.clear();
            app.edit_input2.clear();
        }
        KeyCode::Tab => {
            app.modal_field = 1 - app.modal_field;
        }
        KeyCode::Enter => {
            if !app.edit_input.trim().is_empty() && !app.edit_input2.trim().is_empty() {
                return Action::SaveEditFilterStream;
            }
            if app.edit_input.trim().is_empty() {
                app.modal_field = 0;
            } else {
                app.modal_field = 1;
            }
        }
        KeyCode::Backspace => {
            if app.modal_field == 0 {
                app.edit_input.pop();
            } else {
                app.edit_input2.pop();
            }
        }
        KeyCode::Char(c) => {
            if app.modal_field == 0 {
                app.edit_input.push(c);
            } else {
                app.edit_input2.push(c);
            }
        }
        _ => {}
    }
    Action::None
}

// ── Editor / terminal helpers (TUI-only) ─────────────────────────────────────

/// Suspends TUI is not done here — caller must suspend/restore around this call.
fn run_editor(initial_content: &str) -> anyhow::Result<Option<String>> {
    let cwd = std::env::current_dir()?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let path = cwd.join(format!(".glauca-editor-{}-{nonce}.md", std::process::id()));

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    std::fs::write(&path, initial_content)?;

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    let status = std::process::Command::new(program)
        .args(parts)
        .arg(&path)
        .status()?;

    let result = if status.success() {
        let content = std::fs::read_to_string(&path)?;
        let trimmed = content.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    } else {
        None
    };

    let _ = std::fs::remove_file(&path);
    Ok(result)
}

fn suspend_tui<B: ratatui::backend::Backend + io::Write>(
    terminal: &mut Terminal<B>,
) -> anyhow::Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen
    )?;
    Ok(())
}

fn restore_tui<B: ratatui::backend::Backend + io::Write>(
    terminal: &mut Terminal<B>,
) -> anyhow::Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::EnterAlternateScreen
    )?;
    terminal.clear()?;
    Ok(())
}

// 非同期タスク（run_background_command/execute_*/load_items_task/sync_task/
// sync_worker_task/refresh_timer_task）は glauca_core::engine へ移設（A6）。

// ── Query group reordering helpers ───────────────────────────────────────────

/// Returns the contiguous range `[query_idx, next_query_idx)` for the group
/// starting at `query_idx` (the query entry plus all following filter streams).
struct SelectedEntryLoad {
    entry_id: i64,
    root_id: i64,
    query_str: Option<String>,
    is_filter_stream: bool,
    highlight_since: Option<String>,
    viewed_at: String,
}

fn prepare_selected_entry_load(app: &mut App) -> Option<SelectedEntryLoad> {
    let entry = app.entries.get(app.entry_cursor)?.clone();
    let viewed_at = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let highlight_since = entry.last_viewed_at().map(str::to_string);

    app.stream_filter = entry.stream_filter().map(|s| s.to_string());
    app.active_entry_last_viewed_at = highlight_since.clone();
    if let Some(selected) = app.entries.get_mut(app.entry_cursor) {
        selected.set_last_viewed_at(Some(viewed_at.clone()));
    }
    app.unread_counts.insert(entry.id(), 0);

    Some(SelectedEntryLoad {
        entry_id: entry.id(),
        root_id: entry.root_query_id(),
        query_str: entry.root_query_str().map(str::to_string),
        is_filter_stream: entry.is_filter_stream(),
        highlight_since,
        viewed_at,
    })
}

// `spawn_mark_entry_viewed` は glauca_core::engine へ移設（A6）。

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
    // Build hierarchical left-pane entries: root queries interleaved with their filter streams.
    let query_rows = db::list_queries(&pool).await.unwrap_or_default();
    let mut entries: Vec<LeftPaneEntry> = Vec::new();
    for r in query_rows {
        let streams = db::list_filter_streams(&pool, r.id)
            .await
            .unwrap_or_default();
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

    let (tx, mut rx) = mpsc::channel::<AppMessage>(32);
    let (sync_job_tx, sync_job_rx) = mpsc::channel::<SyncJob>(256);

    // Spawn the sequential background sync worker.
    tokio::spawn(sync_worker_task(
        pool.clone(),
        gh.clone(),
        sync_job_rx,
        tx.clone(),
    ));
    // Spawn the periodic refresh timer (fires every BG_SYNC_INTERVAL_SECS).
    tokio::spawn(refresh_timer_task(
        pool.clone(),
        sync_job_tx.clone(),
        tx.clone(),
    ));

    // Build App from QueryEntry list (filter streams handled via entries above)
    let queries: Vec<QueryEntry> = entries
        .iter()
        .filter_map(|e| {
            if let LeftPaneEntry::Query(q) = e {
                Some(QueryEntry {
                    id: q.id,
                    label: q.label.clone(),
                    query_str: q.query_str.clone(),
                    kind: q.kind.clone(),
                    last_viewed_at: q.last_viewed_at.clone(),
                })
            } else {
                None
            }
        })
        .collect();
    let mut app = App::new(queries);
    app.entries = entries;
    app.current_user = github::get_current_user(&gh).await;

    let root_query_ids: Vec<i64> = app
        .entries
        .iter()
        .filter_map(|entry| match entry {
            LeftPaneEntry::Query(q) => Some(q.id),
            LeftPaneEntry::FilterStream(_) => None,
        })
        .collect();
    for query_id in root_query_ids {
        if let Ok(items) = db::fetch_items(&pool, query_id).await {
            let items = items
                .into_iter()
                .map(|item| cached_item_to_item_entry(item, None))
                .collect::<Vec<_>>();
            app.recompute_unread_counts_for_query(query_id, &items);
        }
    }

    // Helper: spawn cache load + GitHub sync for a root query
    let spawn_load_and_sync = |pool: SqlitePool,
                               gh: Octocrab,
                               query_id: i64,
                               query_str: String,
                               last_viewed_at: Option<String>,
                               tx: mpsc::Sender<AppMessage>| {
        tokio::spawn(load_items_task(
            pool.clone(),
            query_id,
            last_viewed_at.clone(),
            tx.clone(),
        ));
        tokio::spawn(sync_task(pool, gh, query_id, query_str, last_viewed_at, tx));
    };

    // Load items for the initially selected entry; sync only if the cache is stale.
    let mut initially_synced_id: Option<i64> = None;
    if let Some(load) = prepare_selected_entry_load(&mut app) {
        tokio::spawn(load_items_task(
            pool.clone(),
            load.root_id,
            load.highlight_since.clone(),
            tx.clone(),
        ));
        spawn_mark_entry_viewed(
            pool.clone(),
            load.entry_id,
            load.is_filter_stream,
            load.viewed_at.clone(),
            tx.clone(),
        );
        if !load.is_filter_stream {
            if db::is_cache_stale(&pool, load.root_id, CACHE_STALE_SECS)
                .await
                .unwrap_or(true)
            {
                tokio::spawn(sync_task(
                    pool.clone(),
                    gh.clone(),
                    load.root_id,
                    load.query_str.clone().unwrap_or_default(),
                    load.highlight_since.clone(),
                    tx.clone(),
                ));
                app.syncing = true;
            }
            initially_synced_id = Some(load.root_id);
        }
    }

    // Enqueue all other stale queries for immediate background refresh.
    {
        let mut bg_count = 0usize;
        for entry in &app.entries {
            if let LeftPaneEntry::Query(q) = entry {
                if Some(q.id) == initially_synced_id {
                    continue; // already being synced manually
                }
                if db::is_cache_stale(&pool, q.id, CACHE_STALE_SECS)
                    .await
                    .unwrap_or(true)
                {
                    let _ = sync_job_tx
                        .send(SyncJob {
                            query_id: q.id,
                            query_str: q.query_str.clone(),
                        })
                        .await;
                    bg_count += 1;
                }
            }
        }
        app.bg_sync_pending = bg_count;
    }

    let mut events = EventStream::new();

    loop {
        terminal.draw(|f| ui::draw(f, &app))?;

        tokio::select! {
            Some(Ok(event)) = events.next() => {
                if let Event::Key(key) = event {
                    // 'd' in query list → delete selected entry
                    if key.code == KeyCode::Char('d')
                        && app.focus == Focus::QueryList
                        && app.input_mode == InputMode::Normal
                    {
                        if let Some(entry) = app.entries.get(app.entry_cursor) {
                            match entry {
                                LeftPaneEntry::Query(q) => {
                                    let qid = q.id;
                                    if db::delete_query(&pool, qid).await.is_ok() {
                                        // Remove all entries for this root query and its streams
                                        app.entries.retain(|e| {
                                            e.root_query_id() != qid
                                        });
                                        app.entry_cursor = app
                                            .entry_cursor
                                            .min(app.entries.len().saturating_sub(1));
                                        app.items.clear();
                                        app.item_cursor = 0;
                                        app.filter.clear();
                                        app.stream_filter = None;
                                        if let Some(load) = prepare_selected_entry_load(&mut app) {
                                            if !load.is_filter_stream {
                                                spawn_load_and_sync(
                                                    pool.clone(),
                                                    gh.clone(),
                                                    load.root_id,
                                                    load.query_str.clone().unwrap_or_default(),
                                                    load.highlight_since.clone(),
                                                    tx.clone(),
                                                );
                                                app.syncing = true;
                                            } else {
                                                tokio::spawn(load_items_task(
                                                    pool.clone(),
                                                    load.root_id,
                                                    load.highlight_since.clone(),
                                                    tx.clone(),
                                                ));
                                            }
                                            spawn_mark_entry_viewed(
                                                pool.clone(),
                                                load.entry_id,
                                                load.is_filter_stream,
                                                load.viewed_at.clone(),
                                                tx.clone(),
                                            );
                                        }
                                    }
                                }
                                LeftPaneEntry::FilterStream(fs) => {
                                    let fid = fs.id;
                                    if db::delete_filter_stream(&pool, fid).await.is_ok() {
                                        app.entries.retain(|e| e.id() != fid);
                                        app.entry_cursor = app
                                            .entry_cursor
                                            .min(app.entries.len().saturating_sub(1));
                                        app.items.clear();
                                        app.item_cursor = 0;
                                        app.filter.clear();
                                        app.stream_filter = None;
                                        if let Some(load) = prepare_selected_entry_load(&mut app) {
                                            tokio::spawn(load_items_task(
                                                pool.clone(),
                                                load.root_id,
                                                load.highlight_since.clone(),
                                                tx.clone(),
                                            ));
                                            spawn_mark_entry_viewed(
                                                pool.clone(),
                                                load.entry_id,
                                                load.is_filter_stream,
                                                load.viewed_at.clone(),
                                                tx.clone(),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        continue;
                    }

                    // J/K: move selected entry up/down within its group
                    if (key.code == KeyCode::Char('J') || key.code == KeyCode::Char('K'))
                        && app.focus == Focus::QueryList
                        && app.input_mode == InputMode::Normal
                    {
                        let cursor = app.entry_cursor;
                        match app.entries.get(cursor).cloned() {
                            Some(LeftPaneEntry::Query(q)) => {
                                let current_id = q.id;
                                if key.code == KeyCode::Char('J') {
                                    let next_query_idx = group_range(&app.entries, cursor).end;
                                    if let Some(LeftPaneEntry::Query(nq)) =
                                        app.entries.get(next_query_idx)
                                    {
                                        let next_id = nq.id;
                                        if db::swap_query_positions(&pool, current_id, next_id)
                                            .await
                                            .is_ok()
                                        {
                                            if let Some(new_cursor) =
                                                move_group_down(&mut app.entries, cursor)
                                            {
                                                app.entry_cursor = new_cursor;
                                            }
                                        }
                                    }
                                } else {
                                    if let Some(prev_query_idx) = app.entries[..cursor]
                                        .iter()
                                        .rposition(|e| matches!(e, LeftPaneEntry::Query(_)))
                                    {
                                        if let LeftPaneEntry::Query(pq) =
                                            &app.entries[prev_query_idx]
                                        {
                                            let prev_id = pq.id;
                                            if db::swap_query_positions(
                                                &pool, prev_id, current_id,
                                            )
                                            .await
                                            .is_ok()
                                            {
                                                move_group_down(&mut app.entries, prev_query_idx);
                                                app.entry_cursor = prev_query_idx;
                                            }
                                        }
                                    }
                                }
                            }
                            Some(LeftPaneEntry::FilterStream(fs)) => {
                                let fs_id = fs.id;
                                let parent_id = fs.parent_id;
                                if key.code == KeyCode::Char('J') {
                                    // Swap with next sibling (same parent, immediately after).
                                    if let Some(LeftPaneEntry::FilterStream(next)) =
                                        app.entries.get(cursor + 1)
                                    {
                                        if next.parent_id == parent_id {
                                            let next_id = next.id;
                                            if db::swap_filter_stream_positions(
                                                &pool, fs_id, next_id,
                                            )
                                            .await
                                            .is_ok()
                                            {
                                                app.entries.swap(cursor, cursor + 1);
                                                app.entry_cursor += 1;
                                            }
                                        }
                                    }
                                } else if cursor > 0 {
                                    // Swap with previous sibling (same parent, immediately before).
                                    if let Some(LeftPaneEntry::FilterStream(prev)) =
                                        app.entries.get(cursor - 1)
                                    {
                                        if prev.parent_id == parent_id {
                                            let prev_id = prev.id;
                                            if db::swap_filter_stream_positions(
                                                &pool, prev_id, fs_id,
                                            )
                                            .await
                                            .is_ok()
                                            {
                                                app.entries.swap(cursor - 1, cursor);
                                                app.entry_cursor -= 1;
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    let action = handle_key(&mut app, key);
                    match action {
                        Action::Quit => break,
                        Action::LoadEntry => {
                            app.filter.clear();
                            app.item_cursor = 0;
                            app.detail_scroll = 0;
                            app.items.clear();
                            if let Some(load) = prepare_selected_entry_load(&mut app) {
                                if !load.is_filter_stream {
                                    spawn_load_and_sync(
                                        pool.clone(),
                                        gh.clone(),
                                        load.root_id,
                                        load.query_str.clone().unwrap_or_default(),
                                        load.highlight_since.clone(),
                                        tx.clone(),
                                    );
                                    app.syncing = true;
                                } else {
                                    tokio::spawn(load_items_task(
                                        pool.clone(),
                                        load.root_id,
                                        load.highlight_since.clone(),
                                        tx.clone(),
                                    ));
                                }
                                spawn_mark_entry_viewed(
                                    pool.clone(),
                                    load.entry_id,
                                    load.is_filter_stream,
                                    load.viewed_at.clone(),
                                    tx.clone(),
                                );
                            }
                        }
                        Action::SaveNewQuery => {
                            let query_str = app.new_query_input.trim().to_string();
                            let name_str = app.new_query_name.trim().to_string();
                            app.input_mode = InputMode::Normal;
                            app.modal_field = 0;
                            app.new_query_input.clear();
                            app.new_query_name.clear();
                            let pool_clone = pool.clone();
                            let tx_clone = tx.clone();
                            tokio::spawn(async move {
                                let name_opt = if name_str.is_empty() { None } else { Some(name_str.as_str()) };
                                let label = if name_str.is_empty() { query_str.clone() } else { name_str.clone() };
                                match db::upsert_query(&pool_clone, &query_str, "pull_request", name_opt).await {
                                    Ok(id) => {
                                        let _ = tx_clone
                                            .send(AppMessage::QueryAdded(QueryEntry {
                                                id,
                                                label,
                                                query_str,
                                                kind: "pull_request".into(),
                                                last_viewed_at: None,
                                            }))
                                            .await;
                                    }
                                    Err(e) => {
                                        let _ = tx_clone
                                            .send(AppMessage::Status(format!("save error: {e}")))
                                            .await;
                                    }
                                }
                            });
                        }
                        Action::SaveNewFilterStream => {
                            let name = app.new_filter_stream_name.trim().to_string();
                            let filter = app.new_filter_stream_filter.trim().to_string();
                            app.input_mode = InputMode::Normal;
                            app.new_filter_stream_name.clear();
                            app.new_filter_stream_filter.clear();

                            // Determine parent: root_query_id of the currently selected entry
                            if let Some(entry) = app.entries.get(app.entry_cursor) {
                                let parent_id = entry.root_query_id();
                                let kind = entry.kind().to_string();
                                let pool_clone = pool.clone();
                                let tx_clone = tx.clone();
                                tokio::spawn(async move {
                                    match db::upsert_filter_stream(&pool_clone, parent_id, &name, &filter).await {
                                        Ok(id) => {
                                            let _ = tx_clone
                                                .send(AppMessage::FilterStreamAdded(FilterStreamEntry {
                                                    id,
                                                    parent_id,
                                                    name,
                                                    filter,
                                                    kind,
                                                    last_viewed_at: None,
                                                }))
                                                .await;
                                        }
                                        Err(e) => {
                                            let _ = tx_clone
                                                .send(AppMessage::Status(format!("save filter stream error: {e}")))
                                                .await;
                                        }
                                    }
                                });
                            }
                        }
                        Action::SaveEditFilterStream => {
                            let name = app.edit_input.trim().to_string();
                            let filter = app.edit_input2.trim().to_string();
                            app.input_mode = InputMode::Normal;
                            app.edit_input.clear();
                            app.edit_input2.clear();

                            if let Some(LeftPaneEntry::FilterStream(fs)) =
                                app.entries.get(app.entry_cursor)
                            {
                                let id = fs.id;
                                let pool_clone = pool.clone();
                                let tx_clone = tx.clone();
                                tokio::spawn(async move {
                                    match db::update_filter_stream(&pool_clone, id, &name, &filter)
                                        .await
                                    {
                                        Ok(()) => {
                                            let _ = tx_clone
                                                .send(AppMessage::FilterStreamUpdated {
                                                    id,
                                                    new_name: name,
                                                    new_filter: filter,
                                                })
                                                .await;
                                        }
                                        Err(e) => {
                                            let _ = tx_clone
                                                .send(AppMessage::Status(format!(
                                                    "edit filter stream error: {e}"
                                                )))
                                                .await;
                                        }
                                    }
                                });
                            }
                        }
                        Action::SaveEditQuery => {
                            let name_input = app.edit_input.trim().to_string();
                            let new_query = app.edit_input2.trim().to_string();
                            app.input_mode = InputMode::Normal;
                            app.edit_input.clear();
                            app.edit_input2.clear();

                            if let Some(LeftPaneEntry::Query(q)) =
                                app.entries.get(app.entry_cursor)
                            {
                                let id = q.id;
                                // Empty name means "use query string as label"
                                let new_name: Option<String> = if name_input.is_empty() {
                                    None
                                } else {
                                    Some(name_input)
                                };
                                let pool_clone = pool.clone();
                                let tx_clone = tx.clone();
                                let new_name_clone = new_name.clone();
                                tokio::spawn(async move {
                                    match db::update_query(
                                        &pool_clone,
                                        id,
                                        new_name_clone.as_deref(),
                                        &new_query,
                                    )
                                    .await
                                    {
                                        Ok(()) => {
                                            let _ = tx_clone
                                                .send(AppMessage::QueryUpdated {
                                                    id,
                                                    new_name,
                                                    new_query,
                                                })
                                                .await;
                                        }
                                        Err(e) => {
                                            let _ = tx_clone
                                                .send(AppMessage::Status(format!(
                                                    "edit query error: {e}"
                                                )))
                                                .await;
                                        }
                                    }
                                });
                            }
                        }
                        Action::ConfirmAction => {
                            if let Some(item) = app.selected_item().cloned() {
                                let actions = ItemAction::available_for(&item.kind);
                                if let Some(action) = actions.get(app.action_cursor).cloned() {
                                    match action {
                                        ItemAction::OpenBrowser => {
                                            app.input_mode = InputMode::Normal;
                                            let tx_clone = tx.clone();
                                            let item_clone = item.clone();
                                            tokio::spawn(async move {
                                                match execute_open_browser(&item_clone).await {
                                                    Ok(msg) => {
                                                        let _ = tx_clone.send(AppMessage::ActionDone(msg)).await;
                                                    }
                                                    Err(e) => {
                                                        let _ = tx_clone
                                                            .send(AppMessage::ActionError(e.to_string()))
                                                            .await;
                                                    }
                                                }
                                            });
                                        }
                                        ItemAction::Comment => {
                                            app.input_mode = InputMode::Normal;
                                            suspend_tui(terminal)?;
                                            let editor_result = run_editor("");
                                            restore_tui(terminal)?;

                                            match editor_result {
                                                Ok(Some(body)) => {
                                                    let url = item.url.clone();
                                                    let kind = item.kind.clone();
                                                    let tx_clone = tx.clone();
                                                    tokio::spawn(async move {
                                                        match execute_comment(&url, &kind, &body).await {
                                                            Ok(msg) => {
                                                                let _ = tx_clone
                                                                    .send(AppMessage::ActionDone(msg))
                                                                    .await;
                                                            }
                                                            Err(e) => {
                                                                let _ = tx_clone
                                                                    .send(AppMessage::ActionError(e.to_string()))
                                                                    .await;
                                                            }
                                                        }
                                                    });
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
                                            let owner = item.repo_owner.clone();
                                            let repo = item.repo_name.clone();
                                            let number = item.number as u64;
                                            let gh_clone = gh.clone();
                                            let tx_clone = tx.clone();
                                            tokio::spawn(async move {
                                                match fetch_comments_task(&gh_clone, &owner, &repo, number).await {
                                                    Ok(comments) => {
                                                        let _ = tx_clone.send(AppMessage::CommentsLoaded(comments)).await;
                                                    }
                                                    Err(e) => {
                                                        let _ = tx_clone.send(AppMessage::CommentsFailed(e.to_string())).await;
                                                    }
                                                }
                                            });
                                        }
                                        ItemAction::ApprovePR => {
                                            app.input_mode = InputMode::Normal;
                                            suspend_tui(terminal)?;
                                            let editor_result = run_editor(
                                                "# Optional review comment (leave empty to approve without body)\n# Lines starting with '#' are ignored.\n",
                                            );
                                            restore_tui(terminal)?;

                                            match editor_result {
                                                Ok(Some(body)) => {
                                                    let body_final = body
                                                        .lines()
                                                        .filter(|line| !line.starts_with('#'))
                                                        .collect::<Vec<_>>()
                                                        .join("\n");
                                                    let body_final = if body_final.trim().is_empty() {
                                                        None
                                                    } else {
                                                        Some(body_final)
                                                    };
                                                    let url = item.url.clone();
                                                    let tx_clone = tx.clone();
                                                    tokio::spawn(async move {
                                                        match execute_approve(&url, body_final.as_deref()).await {
                                                            Ok(msg) => {
                                                                let _ = tx_clone
                                                                    .send(AppMessage::ActionDone(msg))
                                                                    .await;
                                                            }
                                                            Err(e) => {
                                                                let _ = tx_clone
                                                                    .send(AppMessage::ActionError(e.to_string()))
                                                                    .await;
                                                            }
                                                        }
                                                    });
                                                }
                                                Ok(None) => {
                                                    app.status = Some("Approval cancelled".into());
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
                                    }
                                }
                            }
                        }
                        Action::ConfirmMergeStrategy => {
                            if let Some(item) = app.selected_item().cloned() {
                                let strategies = MergeStrategy::all();
                                if let Some(strategy) = strategies.get(app.merge_strategy_cursor).cloned() {
                                    app.input_mode = InputMode::Normal;
                                    let url = item.url.clone();
                                    let tx_clone = tx.clone();
                                    tokio::spawn(async move {
                                        match execute_merge(&url, &strategy).await {
                                            Ok(msg) => {
                                                let _ = tx_clone.send(AppMessage::ActionDone(msg)).await;
                                            }
                                            Err(e) => {
                                                let _ = tx_clone
                                                    .send(AppMessage::ActionError(e.to_string()))
                                                    .await;
                                            }
                                        }
                                    });
                                }
                            }
                        }
                        Action::OpenBrowser => {
                            if let Some(item) = app.selected_item().cloned() {
                                let tx_clone = tx.clone();
                                tokio::spawn(async move {
                                    match execute_open_browser(&item).await {
                                        Ok(msg) => {
                                            let _ = tx_clone.send(AppMessage::ActionDone(msg)).await;
                                        }
                                        Err(e) => {
                                            let _ = tx_clone
                                                .send(AppMessage::ActionError(e.to_string()))
                                                .await;
                                        }
                                    }
                                });
                            }
                        }
                        Action::None => {}
                    }
                }
            }
            Some(msg) = rx.recv() => {
                match msg {
                    AppMessage::ItemsLoaded { query_id, mut items } => {
                        app.recompute_unread_counts_for_query(query_id, &items);
                        if app.selected_root_query_id() == Some(query_id) {
                            let highlight_since = app.active_entry_last_viewed_at.clone();
                            for item in &mut items {
                                item.is_new = is_item_new_since(&item.cached_at, highlight_since.as_deref());
                            }
                            app.items = items;
                            app.clamp_item_cursor();
                        }
                    }
                    AppMessage::QueryAdded(q) => {
                        app.entries.push(LeftPaneEntry::Query(q));
                        app.entry_cursor = app.entries.len() - 1;
                        app.items.clear();
                        app.filter.clear();
                        app.stream_filter = None;
                        if let Some(load) = prepare_selected_entry_load(&mut app) {
                            spawn_load_and_sync(
                                pool.clone(),
                                gh.clone(),
                                load.root_id,
                                load.query_str.clone().unwrap_or_default(),
                                load.highlight_since.clone(),
                                tx.clone(),
                            );
                            app.syncing = true;
                            spawn_mark_entry_viewed(
                                pool.clone(),
                                load.entry_id,
                                load.is_filter_stream,
                                load.viewed_at.clone(),
                                tx.clone(),
                            );
                        }
                    }
                    AppMessage::FilterStreamAdded(fs) => {
                        // Insert the filter stream after the last sibling (or after its parent)
                        let insert_pos = app
                            .entries
                            .iter()
                            .rposition(|e| e.root_query_id() == fs.parent_id)
                            .map(|p| p + 1)
                            .unwrap_or(app.entries.len());
                        app.entries.insert(insert_pos, LeftPaneEntry::FilterStream(fs));
                        // Select the newly added filter stream
                        app.entry_cursor = insert_pos;
                        app.filter.clear();
                        app.item_cursor = 0;
                        app.detail_scroll = 0;
                        if let Some(load) = prepare_selected_entry_load(&mut app) {
                            tokio::spawn(load_items_task(
                                pool.clone(),
                                load.root_id,
                                load.highlight_since.clone(),
                                tx.clone(),
                            ));
                            spawn_mark_entry_viewed(
                                pool.clone(),
                                load.entry_id,
                                load.is_filter_stream,
                                load.viewed_at.clone(),
                                tx.clone(),
                            );
                        }
                    }
                    AppMessage::QueryUpdated { id, new_name, new_query } => {
                        if let Some(LeftPaneEntry::Query(q)) = app
                            .entries
                            .iter_mut()
                            .find(|e| matches!(e, LeftPaneEntry::Query(q) if q.id == id))
                        {
                            q.label = new_name.clone().unwrap_or_else(|| new_query.clone());
                            q.query_str = new_query.clone();
                        }
                        // Reload + sync with the new query string
                        if app.selected_root_query_id() == Some(id) {
                            app.items.clear();
                            app.item_cursor = 0;
                            app.detail_scroll = 0;
                            app.filter.clear();
                            spawn_load_and_sync(
                                pool.clone(),
                                gh.clone(),
                                id,
                                new_query,
                                app.active_entry_last_viewed_at.clone(),
                                tx.clone(),
                            );
                            app.syncing = true;
                        }
                        app.status = Some("Query updated".into());
                    }
                    AppMessage::FilterStreamUpdated { id, new_name, new_filter } => {
                        if let Some(LeftPaneEntry::FilterStream(fs)) = app
                            .entries
                            .iter_mut()
                            .find(|e| matches!(e, LeftPaneEntry::FilterStream(fs) if fs.id == id))
                        {
                            fs.name = new_name;
                            fs.filter = new_filter.clone();
                        }
                        // If this filter stream is currently selected, re-apply its filter
                        if let Some(LeftPaneEntry::FilterStream(fs)) =
                            app.entries.get(app.entry_cursor)
                        {
                            if fs.id == id {
                                app.stream_filter = Some(new_filter.clone());
                                app.item_cursor = 0;
                                app.detail_scroll = 0;
                                app.clamp_item_cursor();
                            }
                        }
                        if let Some(root_id) = app
                            .entries
                            .iter()
                            .find_map(|entry| match entry {
                                LeftPaneEntry::FilterStream(fs) if fs.id == id => Some(fs.parent_id),
                                _ => None,
                            })
                        {
                            let items = app.items.clone();
                            app.recompute_unread_counts_for_query(root_id, &items);
                        }
                        app.status = Some("Filter stream updated".into());
                    }
                    AppMessage::EntryViewed { entry_id, viewed_at } => {
                        app.mark_entry_viewed(entry_id, viewed_at);
                    }
                    AppMessage::Status(s) => {
                        app.status = Some(s);
                    }
                    AppMessage::ActionDone(msg) => {
                        app.status = Some(msg);
                    }
                    AppMessage::ActionError(err) => {
                        app.status = Some(format!("Error: {err}"));
                    }
                    AppMessage::CommentsLoaded(comments) => {
                        app.comments = comments;
                        app.comments_loading = false;
                    }
                    AppMessage::CommentsFailed(err) => {
                        app.comments_loading = false;
                        // Stay in CommentsPopup so the user sees the error; show it as a comment
                        app.comments = vec![CommentEntry {
                            author: "error".into(),
                            created_at: String::new(),
                            body: format!("Failed to load comments: {err}"),
                            is_minimized: false,
                            minimized_reason: None,
                        }];
                    }
                    AppMessage::SyncDone { query_id, count } => {
                        if app.selected_root_query_id() == Some(query_id) {
                            app.syncing = false;
                            app.status = Some(format!("Synced {count} items"));
                        }
                    }
                    AppMessage::SyncError { query_id, error } => {
                        if app.selected_root_query_id() == Some(query_id) {
                            app.syncing = false;
                        }
                        app.status = Some(format!("Sync error: {error}"));
                    }
                    AppMessage::BgSyncQueued(n) => {
                        app.bg_sync_pending += n;
                    }
                    AppMessage::BgSyncJobDone => {
                        app.bg_sync_pending = app.bg_sync_pending.saturating_sub(1);
                    }
                }
            }
        }
    }

    Ok(())
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_app_with_items(titles: &[&str]) -> App {
        let mut app = App::new(vec![QueryEntry {
            id: 1,
            label: "test query".into(),
            query_str: "test query".into(),
            kind: "pull_request".into(),
            last_viewed_at: None,
        }]);
        app.items = titles
            .iter()
            .enumerate()
            .map(|(i, t)| ItemEntry {
                number: i as i64 + 1,
                title: t.to_string(),
                repo_owner: "owner".into(),
                repo_name: "repo".into(),
                author: Some("alice".into()),
                state: "open".into(),
                updated_at: String::new(),
                labels: vec![],
                url: String::new(),
                comment_count: 0,
                kind: "pull_request".into(),
                requested_reviewers: vec![],
                reviews: vec![],
                body: None,
                assignees: vec![],
                is_draft: false,
                created_at_item: None,
                base_ref: None,
                head_ref: None,
                review_decision: None,
                milestone: None,
                cached_at: String::new(),
                is_new: false,
            })
            .collect();
        app
    }

    #[test]
    fn clamp_item_cursor_when_filter_reduces_list() {
        let mut app = make_app_with_items(&["Fix alpha", "Fix beta", "Add gamma"]);
        app.item_cursor = 2; // points to "Add gamma"

        // Apply filter that matches only 2 items.
        app.filter = "fix".into();
        app.clamp_item_cursor();

        // Cursor should clamp to 1 (last index in filtered list).
        assert!(app.item_cursor <= app.filtered_items().len().saturating_sub(1));
    }

    #[test]
    fn filtered_items_returns_all_when_empty_filter() {
        let app = make_app_with_items(&["Alpha", "Beta", "Gamma"]);
        assert_eq!(app.filtered_items().len(), 3);
    }

    #[test]
    fn filtered_items_plain_text() {
        let mut app = make_app_with_items(&["Fix the bug", "Add feature", "Fix crash"]);
        app.filter = "fix".into();
        let filtered = app.filtered_items();
        assert_eq!(filtered.len(), 2);
        assert!(
            filtered
                .iter()
                .all(|i| i.title.to_lowercase().contains("fix"))
        );
    }

    #[test]
    fn selected_item_follows_cursor() {
        let app = make_app_with_items(&["First", "Second", "Third"]);
        let mut app = app;
        app.item_cursor = 1;
        assert_eq!(
            app.selected_item().map(|i| i.title.as_str()),
            Some("Second")
        );
    }

    #[test]
    fn selected_item_respects_filter() {
        let mut app = make_app_with_items(&["Fix alpha", "Add beta", "Fix gamma"]);
        app.filter = "fix".into();
        app.item_cursor = 1;
        // filtered = ["Fix alpha", "Fix gamma"], cursor=1 → "Fix gamma"
        assert_eq!(
            app.selected_item().map(|i| i.title.as_str()),
            Some("Fix gamma")
        );
    }

    #[test]
    fn selected_item_none_when_list_empty() {
        let app = make_app_with_items(&[]);
        assert!(app.selected_item().is_none());
    }

    #[test]
    fn stream_filter_applied_before_inline_filter() {
        let mut app = make_app_with_items(&["Fix bug", "Add feature", "Fix crash closed"]);
        // Simulate a filter stream that shows only open items
        app.stream_filter = Some("state:open".into());
        // All items have state "open" so all 3 pass stream filter
        assert_eq!(app.filtered_items().len(), 3);

        // Now add inline filter
        app.filter = "fix".into();
        // Only "Fix bug" and "Fix crash closed" match "fix", and all pass stream filter
        assert_eq!(app.filtered_items().len(), 2);
    }

    #[test]
    fn stream_filter_restricts_items() {
        let mut app = App::new(vec![QueryEntry {
            id: 1,
            label: "test".into(),
            query_str: "test".into(),
            kind: "pull_request".into(),
            last_viewed_at: None,
        }]);
        app.items = vec![
            ItemEntry {
                number: 1,
                title: "Open PR".into(),
                repo_owner: "o".into(),
                repo_name: "r".into(),
                author: None,
                state: "open".into(),
                updated_at: String::new(),
                labels: vec![],
                url: String::new(),
                comment_count: 0,
                kind: "pull_request".into(),
                requested_reviewers: vec![],
                reviews: vec![],
                body: None,
                assignees: vec![],
                is_draft: false,
                created_at_item: None,
                base_ref: None,
                head_ref: None,
                review_decision: None,
                milestone: None,
                cached_at: String::new(),
                is_new: false,
            },
            ItemEntry {
                number: 2,
                title: "Closed PR".into(),
                repo_owner: "o".into(),
                repo_name: "r".into(),
                author: None,
                state: "closed".into(),
                updated_at: String::new(),
                labels: vec![],
                url: String::new(),
                comment_count: 0,
                kind: "pull_request".into(),
                requested_reviewers: vec![],
                reviews: vec![],
                body: None,
                assignees: vec![],
                is_draft: false,
                created_at_item: None,
                base_ref: None,
                head_ref: None,
                review_decision: None,
                milestone: None,
                cached_at: String::new(),
                is_new: false,
            },
        ];
        app.stream_filter = Some("state:open".into());
        let filtered = app.filtered_items();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].title, "Open PR");
    }

    #[test]
    fn recompute_unread_counts_uses_entry_last_viewed_at() {
        let mut app = App::new(vec![]);
        app.entries = vec![
            LeftPaneEntry::Query(QueryEntry {
                id: 1,
                label: "Open PRs".into(),
                query_str: "is:pr is:open".into(),
                kind: "pull_request".into(),
                last_viewed_at: Some("2026-05-24 10:00:00".into()),
            }),
            LeftPaneEntry::FilterStream(FilterStreamEntry {
                id: 2,
                parent_id: 1,
                name: "Fresh open".into(),
                filter: "state:open".into(),
                kind: "pull_request".into(),
                last_viewed_at: Some("2026-05-24 10:30:00".into()),
            }),
        ];
        let items = vec![
            ItemEntry {
                number: 1,
                title: "Older open".into(),
                repo_owner: "o".into(),
                repo_name: "r".into(),
                author: None,
                state: "open".into(),
                updated_at: String::new(),
                labels: vec![],
                url: String::new(),
                comment_count: 0,
                kind: "pull_request".into(),
                requested_reviewers: vec![],
                reviews: vec![],
                body: None,
                assignees: vec![],
                is_draft: false,
                created_at_item: None,
                base_ref: None,
                head_ref: None,
                review_decision: None,
                milestone: None,
                cached_at: "2026-05-24 10:15:00".into(),
                is_new: false,
            },
            ItemEntry {
                number: 2,
                title: "Newest open".into(),
                repo_owner: "o".into(),
                repo_name: "r".into(),
                author: None,
                state: "open".into(),
                updated_at: String::new(),
                labels: vec![],
                url: String::new(),
                comment_count: 0,
                kind: "pull_request".into(),
                requested_reviewers: vec![],
                reviews: vec![],
                body: None,
                assignees: vec![],
                is_draft: false,
                created_at_item: None,
                base_ref: None,
                head_ref: None,
                review_decision: None,
                milestone: None,
                cached_at: "2026-05-24 10:45:00".into(),
                is_new: false,
            },
        ];

        app.recompute_unread_counts_for_query(1, &items);

        assert_eq!(app.unread_counts.get(&1), Some(&2));
        assert_eq!(app.unread_counts.get(&2), Some(&1));
    }

    // ── App::new defaults ────────────────────────────────────────────────────────

    #[test]
    fn app_new_default_state() {
        let app = App::new(vec![]);
        assert_eq!(app.focus, Focus::QueryList);
        assert!(matches!(app.input_mode, InputMode::Normal));
        assert!(app.entries.is_empty());
        assert!(app.items.is_empty());
        assert_eq!(app.item_cursor, 0);
        assert_eq!(app.entry_cursor, 0);
        assert!(app.filter.is_empty());
        assert!(app.stream_filter.is_none());
        assert!(!app.syncing);
        assert!(app.current_user.is_none());
    }

    #[test]
    fn app_new_creates_one_entry_per_query() {
        let queries = vec![
            QueryEntry {
                id: 1,
                label: "Open PRs".into(),
                query_str: "is:pr is:open".into(),
                kind: "pull_request".into(),
                last_viewed_at: None,
            },
            QueryEntry {
                id: 2,
                label: "Open issues".into(),
                query_str: "is:issue is:open".into(),
                kind: "issue".into(),
                last_viewed_at: None,
            },
        ];

        let app = App::new(queries);

        assert!(app.items.is_empty());
        assert_eq!(app.entry_cursor, 0);
        assert_eq!(app.item_cursor, 0);
        assert_eq!(app.entries.len(), 2);
        match &app.entries[0] {
            LeftPaneEntry::Query(query) => {
                assert_eq!(query.id, 1);
                assert_eq!(query.label, "Open PRs");
                assert_eq!(query.query_str, "is:pr is:open");
            }
            LeftPaneEntry::FilterStream(_) => panic!("expected query entry"),
        }
        match &app.entries[1] {
            LeftPaneEntry::Query(query) => {
                assert_eq!(query.id, 2);
                assert_eq!(query.label, "Open issues");
                assert_eq!(query.query_str, "is:issue is:open");
            }
            LeftPaneEntry::FilterStream(_) => panic!("expected query entry"),
        }
    }

    // ── expand_me ────────────────────────────────────────────────────────────────

    #[test]
    fn expand_me_author_at_me() {
        let mut app = App::new(vec![]);
        app.current_user = Some("octocat".into());
        assert_eq!(app.expand_me("author:@me"), "author:octocat");
    }

    #[test]
    fn expand_me_review_requested_at_me() {
        let mut app = App::new(vec![]);
        app.current_user = Some("octocat".into());
        assert_eq!(
            app.expand_me("review-requested:@me"),
            "review-requested:octocat"
        );
    }

    #[test]
    fn expand_me_standalone_at_me() {
        let mut app = App::new(vec![]);
        app.current_user = Some("octocat".into());
        assert_eq!(app.expand_me("@me"), "octocat");
    }

    #[test]
    fn expand_me_multiple_tokens() {
        let mut app = App::new(vec![]);
        app.current_user = Some("octocat".into());
        assert_eq!(
            app.expand_me("author:@me review-requested:@me"),
            "author:octocat review-requested:octocat"
        );
    }

    #[test]
    fn expand_me_no_current_user_leaves_unchanged() {
        let app = App::new(vec![]);
        // current_user is None → @me is preserved.
        assert_eq!(app.expand_me("author:@me"), "author:@me");
    }

    #[test]
    fn expand_me_no_at_me_unchanged() {
        let mut app = App::new(vec![]);
        app.current_user = Some("octocat".into());
        let q = "is:pr is:open label:bug";
        assert_eq!(app.expand_me(q), q);
    }

    // ── handle_key_new_query ─────────────────────────────────────────────────────

    fn make_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn make_ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn new_query_enter_on_field0_moves_to_field1() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::NewQuery;
        app.modal_field = 0;
        let action = handle_key_new_query(&mut app, make_key(KeyCode::Enter));
        assert!(matches!(action, Action::None));
        assert_eq!(app.modal_field, 1);
        assert!(matches!(app.input_mode, InputMode::NewQuery));
    }

    #[test]
    fn new_query_enter_on_field1_empty_query_no_save() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::NewQuery;
        app.modal_field = 1;
        app.new_query_input.clear();
        let action = handle_key_new_query(&mut app, make_key(KeyCode::Enter));
        assert!(matches!(action, Action::None));
    }

    #[test]
    fn new_query_enter_on_field1_with_query_saves() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::NewQuery;
        app.modal_field = 1;
        app.new_query_input = "is:pr is:open".into();
        let action = handle_key_new_query(&mut app, make_key(KeyCode::Enter));
        assert!(matches!(action, Action::SaveNewQuery));
    }

    #[test]
    fn new_query_esc_clears_and_exits() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::NewQuery;
        app.modal_field = 1;
        app.new_query_name = "My name".into();
        app.new_query_input = "is:pr".into();
        handle_key_new_query(&mut app, make_key(KeyCode::Esc));
        assert!(matches!(app.input_mode, InputMode::Normal));
        assert!(app.new_query_name.is_empty());
        assert!(app.new_query_input.is_empty());
        assert_eq!(app.modal_field, 0);
    }

    #[test]
    fn new_query_tab_toggles_field() {
        let mut app = App::new(vec![]);
        app.modal_field = 0;
        handle_key_new_query(&mut app, make_key(KeyCode::Tab));
        assert_eq!(app.modal_field, 1);
        handle_key_new_query(&mut app, make_key(KeyCode::Tab));
        assert_eq!(app.modal_field, 0);
    }

    // ── handle_key_filter ────────────────────────────────────────────────────────

    #[test]
    fn filter_esc_exits_mode() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::Filter;
        handle_key_filter(&mut app, make_key(KeyCode::Esc));
        assert!(matches!(app.input_mode, InputMode::Normal));
    }

    #[test]
    fn filter_backspace_removes_last_char() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::Filter;
        app.filter = "fix".into();
        handle_key_filter(&mut app, make_key(KeyCode::Backspace));
        assert_eq!(app.filter, "fi");
    }

    #[test]
    fn filter_ctrl_u_clears_filter() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::Filter;
        app.filter = "some filter text".into();
        handle_key_filter(&mut app, make_ctrl_key(KeyCode::Char('u')));
        assert!(app.filter.is_empty());
    }

    #[test]
    fn filter_char_appends() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::Filter;
        app.filter = "fi".into();
        handle_key_filter(&mut app, make_key(KeyCode::Char('x')));
        assert_eq!(app.filter, "fix");
    }

    #[test]
    fn app_new_initializes_action_state() {
        let app = App::new(vec![]);
        assert_eq!(app.action_cursor, 0);
        assert_eq!(app.merge_strategy_cursor, 0);
    }

    #[test]
    fn item_action_available_for_kind_is_context_aware() {
        assert_eq!(
            ItemAction::available_for("pull_request"),
            vec![
                ItemAction::OpenBrowser,
                ItemAction::Comment,
                ItemAction::ViewComments,
                ItemAction::ApprovePR,
                ItemAction::MergePR,
            ]
        );
        assert_eq!(
            ItemAction::available_for("issue"),
            vec![
                ItemAction::OpenBrowser,
                ItemAction::Comment,
                ItemAction::ViewComments,
            ]
        );
    }

    #[test]
    fn enter_opens_action_menu_for_selected_item() {
        let mut app = make_app_with_items(&["First"]);
        app.focus = Focus::ItemList;

        let action = handle_key_normal(&mut app, make_key(KeyCode::Enter));

        assert!(matches!(action, Action::None));
        assert_eq!(app.input_mode, InputMode::ActionMenu);
        assert_eq!(app.action_cursor, 0);
    }

    #[test]
    fn action_menu_navigation_and_confirm_work() {
        let mut app = make_app_with_items(&["First"]);
        app.input_mode = InputMode::ActionMenu;

        handle_key_action_menu(&mut app, make_key(KeyCode::Down));
        assert_eq!(app.action_cursor, 1);

        let action = handle_key_action_menu(&mut app, make_key(KeyCode::Enter));
        assert!(matches!(action, Action::ConfirmAction));
    }

    #[test]
    fn merge_menu_escape_returns_to_action_menu() {
        let mut app = make_app_with_items(&["First"]);
        app.input_mode = InputMode::MergeMenu;
        app.merge_strategy_cursor = 1;

        let action = handle_key_merge_menu(&mut app, make_key(KeyCode::Esc));

        assert!(matches!(action, Action::None));
        assert_eq!(app.input_mode, InputMode::ActionMenu);
        assert_eq!(app.merge_strategy_cursor, 1);
    }
}
