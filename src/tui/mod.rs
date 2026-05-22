use anyhow::Result;
use crate::{db, github};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use octocrab::Octocrab;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use sqlx::SqlitePool;
use std::io;
use tokio::sync::mpsc;

pub mod filter;
pub mod ui;

use filter::FilterQuery;

// ── Display structs used by the TUI ─────────────────────────────────────────

pub struct QueryEntry {
    pub id: i64,
    pub label: String,
    pub kind: String,
}

pub struct FilterStreamEntry {
    pub id: i64,
    pub parent_id: i64,
    pub name: String,
    pub filter: String,
    pub kind: String,
}

/// A single row in the left pane — either a root query or a filter stream.
pub enum LeftPaneEntry {
    Query(QueryEntry),
    FilterStream(FilterStreamEntry),
}

impl LeftPaneEntry {
    pub fn id(&self) -> i64 {
        match self {
            Self::Query(q) => q.id,
            Self::FilterStream(fs) => fs.id,
        }
    }

    pub fn display_label(&self) -> &str {
        match self {
            Self::Query(q) => &q.label,
            Self::FilterStream(fs) => &fs.name,
        }
    }

    pub fn kind(&self) -> &str {
        match self {
            Self::Query(q) => &q.kind,
            Self::FilterStream(fs) => &fs.kind,
        }
    }

    /// The root query whose cached items should be loaded.
    pub fn root_query_id(&self) -> i64 {
        match self {
            Self::Query(q) => q.id,
            Self::FilterStream(fs) => fs.parent_id,
        }
    }

    /// Filter string to apply on top of the inline filter (None for root queries).
    pub fn stream_filter(&self) -> Option<&str> {
        match self {
            Self::Query(_) => None,
            Self::FilterStream(fs) => Some(&fs.filter),
        }
    }

    pub fn is_filter_stream(&self) -> bool {
        matches!(self, Self::FilterStream(_))
    }

    pub fn parent_id(&self) -> Option<i64> {
        match self {
            Self::Query(_) => None,
            Self::FilterStream(fs) => Some(fs.parent_id),
        }
    }
}

#[derive(Clone)]
pub struct ItemEntry {
    pub number: i64,
    pub title: String,
    pub repo_owner: String,
    pub repo_name: String,
    pub author: Option<String>,
    pub state: String,
    pub updated_at: String,
    pub labels: Vec<String>,
    pub url: String,
    pub comment_count: i64,
}

// ── Application state ────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
pub enum Focus {
    QueryList,
    ItemList,
    ItemDetail,
}

#[derive(PartialEq)]
pub enum InputMode {
    Normal,
    Filter,
    NewQuery,
    /// Step 1: entering display name for a new filter stream.
    NewFilterStreamName,
    /// Step 2: entering filter string for a new filter stream.
    NewFilterStreamFilter,
}

pub struct App {
    pub focus: Focus,
    pub input_mode: InputMode,

    pub entries: Vec<LeftPaneEntry>,
    pub entry_cursor: usize,

    pub items: Vec<ItemEntry>,
    pub item_cursor: usize,
    pub filter: String,
    /// Active filter stream filter applied before the inline filter (if any).
    pub stream_filter: Option<String>,

    pub new_query_input: String,
    pub new_filter_stream_name: String,
    pub new_filter_stream_filter: String,
    pub status: Option<String>,
    /// Whether a background GitHub sync is in progress.
    pub syncing: bool,
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
            filter: String::new(),
            stream_filter: None,
            new_query_input: String::new(),
            new_filter_stream_name: String::new(),
            new_filter_stream_filter: String::new(),
            status: None,
            syncing: false,
        }
    }

    pub fn parsed_filter(&self) -> FilterQuery {
        FilterQuery::parse(&self.filter)
    }

    pub fn filtered_items(&self) -> Vec<&ItemEntry> {
        let stream_q = self
            .stream_filter
            .as_deref()
            .map(FilterQuery::parse);
        let inline_q = self.parsed_filter();

        self.items
            .iter()
            .filter(|i| {
                stream_q.as_ref().map_or(true, |q| q.matches(i))
                    && (inline_q.is_empty() || inline_q.matches(i))
            })
            .collect()
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

    /// Apply the selected entry's stream filter and return the root query_id.
    fn activate_selected_entry(&mut self) -> Option<i64> {
        if let Some(entry) = self.entries.get(self.entry_cursor) {
            self.stream_filter = entry.stream_filter().map(|s| s.to_string());
            Some(entry.root_query_id())
        } else {
            None
        }
    }
}

// ── Background messages ──────────────────────────────────────────────────────

pub enum AppMessage {
    ItemsLoaded { query_id: i64, items: Vec<ItemEntry> },
    QueryAdded(QueryEntry),
    FilterStreamAdded(FilterStreamEntry),
    Status(String),
    SyncDone { query_id: i64, count: usize },
    SyncError { query_id: i64, error: String },
}

// ── Actions returned from key handling ───────────────────────────────────────

enum Action {
    None,
    Quit,
    LoadEntry,
    SaveNewQuery,
    SaveNewFilterStream,
}

// ── Key event handler ────────────────────────────────────────────────────────

fn handle_key(app: &mut App, key: KeyEvent) -> Action {
    match app.input_mode {
        InputMode::Filter => handle_key_filter(app, key),
        InputMode::NewQuery => handle_key_new_query(app, key),
        InputMode::NewFilterStreamName => handle_key_new_fs_name(app, key),
        InputMode::NewFilterStreamFilter => handle_key_new_fs_filter(app, key),
        InputMode::Normal => handle_key_normal(app, key),
    }
}

fn handle_key_normal(app: &mut App, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('q') => return Action::Quit,

        // Focus cycling
        KeyCode::Tab => {
            app.focus = match app.focus {
                Focus::QueryList => Focus::ItemList,
                Focus::ItemList => Focus::ItemDetail,
                Focus::ItemDetail => Focus::QueryList,
            };
        }
        KeyCode::BackTab => {
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
            Focus::ItemList | Focus::ItemDetail => {
                let max = app.filtered_items().len().saturating_sub(1);
                if app.item_cursor < max {
                    app.item_cursor += 1;
                }
            }
        },
        KeyCode::Char('k') | KeyCode::Up => match app.focus {
            Focus::QueryList => {
                if app.entry_cursor > 0 {
                    app.entry_cursor -= 1;
                    return Action::LoadEntry;
                }
            }
            Focus::ItemList | Focus::ItemDetail => {
                if app.item_cursor > 0 {
                    app.item_cursor -= 1;
                }
            }
        },

        // New root query (left pane)
        KeyCode::Char('n') if app.focus == Focus::QueryList => {
            app.input_mode = InputMode::NewQuery;
            app.new_query_input.clear();
        }
        // New filter stream (left pane) — only when a root query or filter stream is selected
        KeyCode::Char('f') if app.focus == Focus::QueryList => {
            if !app.entries.is_empty() {
                app.input_mode = InputMode::NewFilterStreamName;
                app.new_filter_stream_name.clear();
                app.new_filter_stream_filter.clear();
            }
        }
        // Delete handled in main loop
        KeyCode::Char('d')
            if app.focus == Focus::QueryList
                && key.modifiers.contains(KeyModifiers::NONE) => {}

        // Enter filter mode (middle pane)
        KeyCode::Char('/') if app.focus == Focus::ItemList => {
            app.input_mode = InputMode::Filter;
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
    match key.code {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
            app.new_query_input.clear();
        }
        KeyCode::Enter => {
            if !app.new_query_input.trim().is_empty() {
                return Action::SaveNewQuery;
            }
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace => {
            app.new_query_input.pop();
        }
        KeyCode::Char(c) => {
            app.new_query_input.push(c);
        }
        _ => {}
    }
    Action::None
}

fn handle_key_new_fs_name(app: &mut App, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
            app.new_filter_stream_name.clear();
        }
        KeyCode::Enter => {
            if !app.new_filter_stream_name.trim().is_empty() {
                // Advance to step 2: enter filter string
                app.input_mode = InputMode::NewFilterStreamFilter;
            }
        }
        KeyCode::Backspace => {
            app.new_filter_stream_name.pop();
        }
        KeyCode::Char(c) => {
            app.new_filter_stream_name.push(c);
        }
        _ => {}
    }
    Action::None
}

fn handle_key_new_fs_filter(app: &mut App, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
            app.new_filter_stream_name.clear();
            app.new_filter_stream_filter.clear();
        }
        KeyCode::Enter => {
            if !app.new_filter_stream_filter.trim().is_empty() {
                return Action::SaveNewFilterStream;
            }
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace => {
            app.new_filter_stream_filter.pop();
        }
        KeyCode::Char(c) => {
            app.new_filter_stream_filter.push(c);
        }
        _ => {}
    }
    Action::None
}

// ── Background task helpers ───────────────────────────────────────────────────

async fn load_items_task(pool: SqlitePool, query_id: i64, tx: mpsc::Sender<AppMessage>) {
    match db::fetch_items(&pool, query_id).await {
        Ok(cached) => {
            let items = cached
                .into_iter()
                .map(|c| ItemEntry {
                    number: c.number,
                    title: c.title,
                    repo_owner: c.repo_owner,
                    repo_name: c.repo_name,
                    author: c.author,
                    state: c.state,
                    updated_at: c.updated_at,
                    labels: serde_labels(&c.labels),
                    url: c.url,
                    comment_count: c.comment_count,
                })
                .collect();
            let _ = tx.send(AppMessage::ItemsLoaded { query_id, items }).await;
        }
        Err(e) => {
            let _ = tx.send(AppMessage::Status(format!("load error: {e}"))).await;
        }
    }
}

fn serde_labels(raw: &str) -> Vec<String> {
    // Labels are stored as a JSON array string, e.g. '["bug","enhancement"]'
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

/// Fetch fresh results from GitHub API, upsert into SQLite, then reload into TUI.
async fn sync_task(
    pool: SqlitePool,
    gh: Octocrab,
    query_id: i64,
    query_str: String,
    tx: mpsc::Sender<AppMessage>,
) {
    match github::search(&gh, query_id, &query_str).await {
        Ok(fetched) => {
            let count = fetched.len();
            for item in &fetched {
                if let Err(e) = db::upsert_item(&pool, item).await {
                    let _ = tx
                        .send(AppMessage::SyncError {
                            query_id,
                            error: format!("db write error: {e}"),
                        })
                        .await;
                    return;
                }
            }
            if let Err(e) = db::mark_fetched(&pool, query_id).await {
                let _ = tx
                    .send(AppMessage::SyncError {
                        query_id,
                        error: format!("mark fetched error: {e}"),
                    })
                    .await;
                return;
            }
            // Reload from DB so the TUI shows consistent cached data.
            let _ = tx
                .send(AppMessage::SyncDone { query_id, count })
                .await;
            load_items_task(pool, query_id, tx).await;
        }
        Err(e) => {
            let _ = tx
                .send(AppMessage::SyncError {
                    query_id,
                    error: format!("GitHub API error: {e}"),
                })
                .await;
        }
    }
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

async fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    pool: SqlitePool,
    gh: Octocrab,
) -> Result<()> {
    // Build hierarchical left-pane entries: root queries interleaved with their filter streams.
    let query_rows = db::list_queries(&pool).await.unwrap_or_default();
    let mut entries: Vec<LeftPaneEntry> = Vec::new();
    for r in query_rows {
        let streams = db::list_filter_streams(&pool, r.id).await.unwrap_or_default();
        let kind = r.kind.clone();
        entries.push(LeftPaneEntry::Query(QueryEntry {
            id: r.id,
            label: r.query,
            kind: kind.clone(),
        }));
        for s in streams {
            entries.push(LeftPaneEntry::FilterStream(FilterStreamEntry {
                id: s.id,
                parent_id: s.parent_id,
                name: s.name,
                filter: s.filter,
                kind: kind.clone(),
            }));
        }
    }

    let (tx, mut rx) = mpsc::channel::<AppMessage>(32);

    // Build App from QueryEntry list (filter streams handled via entries above)
    let queries: Vec<QueryEntry> = entries
        .iter()
        .filter_map(|e| {
            if let LeftPaneEntry::Query(q) = e {
                Some(QueryEntry { id: q.id, label: q.label.clone(), kind: q.kind.clone() })
            } else {
                None
            }
        })
        .collect();
    let mut app = App::new(queries);
    app.entries = entries;

    // Helper: spawn cache load + GitHub sync for a root query
    let spawn_load_and_sync = |pool: SqlitePool,
                               gh: Octocrab,
                               query_id: i64,
                               query_str: String,
                               tx: mpsc::Sender<AppMessage>| {
        tokio::spawn(load_items_task(pool.clone(), query_id, tx.clone()));
        tokio::spawn(sync_task(pool, gh, query_id, query_str, tx));
    };

    // Load items for the initially selected entry
    if let Some(root_id) = app.activate_selected_entry() {
        if let Some(entry) = app.entries.first() {
            if !entry.is_filter_stream() {
                let query_str = entry.display_label().to_string();
                spawn_load_and_sync(pool.clone(), gh.clone(), root_id, query_str, tx.clone());
                app.syncing = true;
            } else {
                tokio::spawn(load_items_task(pool.clone(), root_id, tx.clone()));
            }
        }
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
                                        if let Some(root_id) = app.activate_selected_entry() {
                                            if let Some(e) = app.entries.get(app.entry_cursor) {
                                                if !e.is_filter_stream() {
                                                    let qs = e.display_label().to_string();
                                                    spawn_load_and_sync(
                                                        pool.clone(), gh.clone(), root_id, qs, tx.clone(),
                                                    );
                                                    app.syncing = true;
                                                } else {
                                                    tokio::spawn(load_items_task(pool.clone(), root_id, tx.clone()));
                                                }
                                            }
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
                                        if let Some(root_id) = app.activate_selected_entry() {
                                            tokio::spawn(load_items_task(pool.clone(), root_id, tx.clone()));
                                        }
                                    }
                                }
                            }
                        }
                        continue;
                    }

                    let action = handle_key(&mut app, key);
                    match action {
                        Action::Quit => break,
                        Action::LoadEntry => {
                            app.filter.clear();
                            app.item_cursor = 0;
                            app.items.clear();
                            if let Some(root_id) = app.activate_selected_entry() {
                                if let Some(e) = app.entries.get(app.entry_cursor) {
                                    if !e.is_filter_stream() {
                                        let qs = e.display_label().to_string();
                                        spawn_load_and_sync(
                                            pool.clone(), gh.clone(), root_id, qs, tx.clone(),
                                        );
                                        app.syncing = true;
                                    } else {
                                        tokio::spawn(load_items_task(pool.clone(), root_id, tx.clone()));
                                    }
                                }
                            }
                        }
                        Action::SaveNewQuery => {
                            let query_str = app.new_query_input.trim().to_string();
                            app.input_mode = InputMode::Normal;
                            app.new_query_input.clear();
                            let pool_clone = pool.clone();
                            let tx_clone = tx.clone();
                            tokio::spawn(async move {
                                match db::upsert_query(&pool_clone, &query_str, "pull_request").await {
                                    Ok(id) => {
                                        let _ = tx_clone
                                            .send(AppMessage::QueryAdded(QueryEntry {
                                                id,
                                                label: query_str,
                                                kind: "pull_request".into(),
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
                        Action::None => {}
                    }
                }
            }
            Some(msg) = rx.recv() => {
                match msg {
                    AppMessage::ItemsLoaded { query_id, items } => {
                        if app.selected_root_query_id() == Some(query_id) {
                            app.items = items;
                            app.clamp_item_cursor();
                        }
                    }
                    AppMessage::QueryAdded(q) => {
                        let load_id = q.id;
                        let query_str = q.label.clone();
                        let kind = q.kind.clone();
                        app.entries.push(LeftPaneEntry::Query(QueryEntry {
                            id: q.id,
                            label: q.label,
                            kind,
                        }));
                        app.entry_cursor = app.entries.len() - 1;
                        app.items.clear();
                        app.filter.clear();
                        app.stream_filter = None;
                        spawn_load_and_sync(
                            pool.clone(),
                            gh.clone(),
                            load_id,
                            query_str,
                            tx.clone(),
                        );
                        app.syncing = true;
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
                        if let Some(root_id) = app.activate_selected_entry() {
                            tokio::spawn(load_items_task(pool.clone(), root_id, tx.clone()));
                        }
                    }
                    AppMessage::Status(s) => {
                        app.status = Some(s);
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
            kind: "pull_request".into(),
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
        assert!(filtered.iter().all(|i| i.title.to_lowercase().contains("fix")));
    }

    #[test]
    fn selected_item_follows_cursor() {
        let app = make_app_with_items(&["First", "Second", "Third"]);
        let mut app = app;
        app.item_cursor = 1;
        assert_eq!(app.selected_item().map(|i| i.title.as_str()), Some("Second"));
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
            kind: "pull_request".into(),
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
            },
        ];
        app.stream_filter = Some("state:open".into());
        let filtered = app.filtered_items();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].title, "Open PR");
    }
}

