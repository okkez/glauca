// glauca-core::engine — framework 非依存の非同期エンジン処理。
// TUI/GUI 双方から利用する。$EDITOR 起動・端末制御などフロントエンド固有の処理は呼び出し側に残し、
// ここには pool/gh/mpsc チャネルだけで完結するタスクと、それらが受け渡すメッセージ型を集約する。

use crate::filter::FilterQuery;
use crate::logic::cached_item_to_item_entry;
use crate::types::{
    CommentEntry, FilterStreamEntry, ItemEntry, LeftPaneEntry, MergeStrategy, QueryEntry,
};
use crate::{db, github};
use octocrab::Octocrab;
use sqlx::SqlitePool;
use std::process::Stdio;
use tokio::sync::mpsc;

// ── Background messages ──────────────────────────────────────────────────────

pub enum AppMessage {
    ItemsLoaded {
        query_id: i64,
        items: Vec<ItemEntry>,
    },
    QueryAdded(QueryEntry),
    FilterStreamAdded(FilterStreamEntry),
    QueryUpdated {
        id: i64,
        new_name: Option<String>,
        new_query: String,
    },
    FilterStreamUpdated {
        id: i64,
        new_name: String,
        new_filter: String,
    },
    EntryViewed {
        entry_id: i64,
        viewed_at: String,
    },
    Status(String),
    ActionDone(String),
    ActionError(String),
    CommentsLoaded(Vec<CommentEntry>),
    CommentsFailed(String),
    SyncDone {
        query_id: i64,
        count: usize,
    },
    SyncError {
        query_id: i64,
        error: String,
    },
    /// N background sync jobs were added to the worker queue.
    BgSyncQueued(usize),
    /// One background sync job finished (success or skip).
    BgSyncJobDone,
    /// A GitHub sync actually started for `query_id` (drives the syncing indicator
    /// for paths where the decision is made asynchronously, e.g. sync-if-stale).
    SyncStarted {
        query_id: i64,
    },
    /// A root query (and its filter streams) was deleted from the DB.
    QueryDeleted {
        query_id: i64,
    },
    /// A filter stream was deleted from the DB.
    FilterStreamDeleted {
        id: i64,
    },
    /// Two query groups swapped position in the DB; `active_id` is the query the
    /// cursor should follow after the front-end reorders its entries.
    QueriesSwapped {
        upper_id: i64,
        lower_id: i64,
        active_id: i64,
    },
    /// Two sibling filter streams swapped position in the DB; `active_id` is the
    /// filter stream the cursor should follow.
    FilterStreamsSwapped {
        upper_id: i64,
        lower_id: i64,
        active_id: i64,
    },
}

/// A job request sent to the background sync worker.
pub struct SyncJob {
    pub query_id: i64,
    pub query_str: String,
}

// ── Background task helpers ───────────────────────────────────────────────────

/// Fetch issue/PR comments from GitHub API (up to 100 most recent).
pub async fn fetch_comments_task(
    gh: &Octocrab,
    owner: &str,
    repo: &str,
    number: u64,
) -> anyhow::Result<Vec<CommentEntry>> {
    let query = r#"
        query($owner: String!, $repo: String!, $number: Int!) {
          repository(owner: $owner, name: $repo) {
            issueOrPullRequest(number: $number) {
              ... on Issue {
                comments(first: 100) {
                  nodes {
                    author { login }
                    body
                    createdAt
                    isMinimized
                    minimizedReason
                  }
                }
              }
              ... on PullRequest {
                comments(first: 100) {
                  nodes {
                    author { login }
                    body
                    createdAt
                    isMinimized
                    minimizedReason
                  }
                }
              }
            }
          }
        }
    "#;
    let payload = serde_json::json!({
        "query": query,
        "variables": {
            "owner": owner,
            "repo": repo,
            "number": number as i64,
        }
    });
    let resp: serde_json::Value = gh.graphql(&payload).await?;
    let nodes = resp
        .pointer("/data/repository/issueOrPullRequest/comments/nodes")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let comments = nodes
        .into_iter()
        .map(|n| {
            let author = n
                .pointer("/author/login")
                .and_then(|v| v.as_str())
                .unwrap_or("ghost")
                .to_string();
            let created_at = n
                .get("createdAt")
                .and_then(|v| v.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default();
            let body = n
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let is_minimized = n
                .get("isMinimized")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let minimized_reason = n
                .get("minimizedReason")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            CommentEntry {
                author,
                created_at,
                body,
                is_minimized,
                minimized_reason,
            }
        })
        .collect();
    Ok(comments)
}

async fn run_background_command(
    mut cmd: tokio::process::Command,
    failure: &str,
) -> anyhow::Result<()> {
    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            Err(anyhow::anyhow!(failure.to_string()))
        } else {
            Err(anyhow::anyhow!(format!("{failure}: {stderr}")))
        }
    }
}

pub async fn execute_open_browser(item: &ItemEntry) -> anyhow::Result<String> {
    let sub = if item.kind == "pull_request" {
        "pr"
    } else {
        "issue"
    };
    let repo = format!("{}/{}", item.repo_owner, item.repo_name);
    let mut cmd = tokio::process::Command::new("gh");
    cmd.args([sub, "view", "--web", &item.number.to_string(), "-R", &repo]);
    run_background_command(cmd, "Failed to open in browser").await?;
    Ok("Opened in browser".into())
}

pub async fn execute_comment(url: &str, kind: &str, body: &str) -> anyhow::Result<String> {
    let sub = if kind == "pull_request" {
        "pr"
    } else {
        "issue"
    };
    let mut cmd = tokio::process::Command::new("gh");
    cmd.args([sub, "comment", url, "--body", body]);
    run_background_command(cmd, &format!("gh {sub} comment failed")).await?;
    Ok("Comment posted".into())
}

pub async fn execute_approve(url: &str, body: Option<&str>) -> anyhow::Result<String> {
    let mut cmd = tokio::process::Command::new("gh");
    cmd.args(["pr", "review", "--approve", url]);
    if let Some(b) = body {
        cmd.args(["-b", b]);
    }
    run_background_command(cmd, "gh pr review --approve failed").await?;
    Ok("PR approved".into())
}

pub async fn execute_merge(url: &str, strategy: &MergeStrategy) -> anyhow::Result<String> {
    let mut cmd = tokio::process::Command::new("gh");
    cmd.args(["pr", "merge", strategy.flag(), url]);
    run_background_command(cmd, "gh pr merge failed").await?;
    Ok(format!("PR merged ({})", strategy.label()))
}

pub async fn load_items_task(
    pool: SqlitePool,
    query_id: i64,
    last_viewed_at: Option<String>,
    tx: mpsc::Sender<AppMessage>,
) {
    match db::fetch_items(&pool, query_id).await {
        Ok(cached) => {
            let items = cached
                .into_iter()
                .map(|c| cached_item_to_item_entry(c, last_viewed_at.as_deref()))
                .collect();
            let _ = tx.send(AppMessage::ItemsLoaded { query_id, items }).await;
        }
        Err(e) => {
            let _ = tx
                .send(AppMessage::Status(format!("load error: {e}")))
                .await;
        }
    }
}

/// Fetch fresh results from GitHub API page by page, upserting each page immediately
/// so the UI can show results as they arrive rather than waiting for all pages.
pub async fn sync_task(
    pool: SqlitePool,
    gh: Octocrab,
    query_id: i64,
    query_str: String,
    last_viewed_at: Option<String>,
    tx: mpsc::Sender<AppMessage>,
) {
    let mut after: Option<String> = None;
    let mut total_count = 0usize;

    loop {
        let result = github::search_page(&gh, query_id, &query_str, after.as_deref()).await;
        match result {
            Err(e) => {
                let _ = tx
                    .send(AppMessage::SyncError {
                        query_id,
                        error: format!("GitHub API error: {e}"),
                    })
                    .await;
                return;
            }
            Ok(page) => {
                let has_next = page.has_next_page;
                let cursor = page.end_cursor.clone();

                // Upsert this page's items into SQLite.
                for item in &page.items {
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
                total_count += page.items.len();

                // Reload from DB after each page so the UI shows results immediately.
                load_items_task(pool.clone(), query_id, last_viewed_at.clone(), tx.clone()).await;

                if !has_next {
                    break;
                }
                after = cursor;
                if after.is_none() {
                    break;
                }
            }
        }
    }

    // Mark the query as freshly fetched only after all pages are done.
    if let Err(e) = db::mark_fetched(&pool, query_id).await {
        let _ = tx
            .send(AppMessage::SyncError {
                query_id,
                error: format!("mark fetched error: {e}"),
            })
            .await;
        return;
    }
    let _ = tx
        .send(AppMessage::SyncDone {
            query_id,
            count: total_count,
        })
        .await;
}

// ── Background sync worker & refresh timer ────────────────────────────────────

/// Auto-refresh interval and cache staleness threshold (seconds).
const BG_SYNC_INTERVAL_SECS: u64 = 300;
pub const CACHE_STALE_SECS: i64 = 300;

/// Processes `SyncJob`s sequentially, skipping jobs whose cache is already fresh.
/// Sends `BgSyncJobDone` after each job (regardless of outcome) so the UI counter
/// stays accurate.
pub async fn sync_worker_task(
    pool: SqlitePool,
    gh: Octocrab,
    mut rx: mpsc::Receiver<SyncJob>,
    tx: mpsc::Sender<AppMessage>,
) {
    while let Some(job) = rx.recv().await {
        let stale = db::is_cache_stale(&pool, job.query_id, CACHE_STALE_SECS)
            .await
            .unwrap_or(true);
        if stale {
            sync_task(
                pool.clone(),
                gh.clone(),
                job.query_id,
                job.query_str,
                None,
                tx.clone(),
            )
            .await;
        }
        let _ = tx.send(AppMessage::BgSyncJobDone).await;
    }
}

/// Every `BG_SYNC_INTERVAL_SECS`, enqueues all stale queries to the worker.
pub async fn refresh_timer_task(
    pool: SqlitePool,
    sync_tx: mpsc::Sender<SyncJob>,
    app_tx: mpsc::Sender<AppMessage>,
) {
    let mut interval =
        tokio::time::interval(tokio::time::Duration::from_secs(BG_SYNC_INTERVAL_SECS));
    interval.tick().await; // skip the immediate first tick
    loop {
        interval.tick().await;
        let queries = db::list_queries(&pool).await.unwrap_or_default();
        let mut count = 0usize;
        for q in queries {
            if db::is_cache_stale(&pool, q.id, CACHE_STALE_SECS)
                .await
                .unwrap_or(true)
            {
                let _ = sync_tx
                    .send(SyncJob {
                        query_id: q.id,
                        query_str: q.query,
                    })
                    .await;
                count += 1;
            }
        }
        if count > 0 {
            let _ = app_tx.send(AppMessage::BgSyncQueued(count)).await;
        }
    }
}

pub fn spawn_mark_entry_viewed(
    pool: SqlitePool,
    entry_id: i64,
    is_filter_stream: bool,
    viewed_at: String,
    tx: mpsc::Sender<AppMessage>,
) {
    tokio::spawn(async move {
        let result = if is_filter_stream {
            db::mark_filter_stream_viewed(&pool, entry_id).await
        } else {
            db::mark_query_viewed(&pool, entry_id).await
        };

        match result {
            Ok(()) => {
                let _ = tx
                    .send(AppMessage::EntryViewed {
                        entry_id,
                        viewed_at,
                    })
                    .await;
            }
            Err(e) => {
                let _ = tx
                    .send(AppMessage::Status(format!("mark viewed error: {e}")))
                    .await;
            }
        }
    });
}

// ── Engine: command-driven async facade ───────────────────────────────────────

/// Initial state produced by `Engine::start`: the left-pane entries (root queries
/// interleaved with their filter streams) and the authenticated user login.
pub struct EngineInit {
    pub entries: Vec<LeftPaneEntry>,
    pub current_user: Option<String>,
    /// Display name of the authenticated user (GitHub `name`), if set.
    pub current_user_name: Option<String>,
    /// Avatar URL of the authenticated user.
    pub current_user_avatar_url: Option<String>,
}

/// Commands the front-end sends to the engine. Each is handled asynchronously and,
/// where the UI must react, produces an `AppMessage` in return. The front-end owns
/// UI state (entries vec, cursors); the engine owns all DB/network/spawn work.
pub enum EngineCommand {
    /// Load cached items for a root query (no GitHub sync).
    LoadCached {
        query_id: i64,
        highlight_since: Option<String>,
    },
    /// Unconditional GitHub sync for a root query.
    Sync {
        query_id: i64,
        query_str: String,
        highlight_since: Option<String>,
    },
    /// GitHub sync only if the query's cache is stale (sends `SyncStarted` if it runs).
    SyncIfStale {
        query_id: i64,
        query_str: String,
        highlight_since: Option<String>,
    },
    /// Re-fetch a single item from GitHub and upsert it into `query_id`'s cache,
    /// then reload that query's items. Used by the per-item refresh action.
    RefreshItem {
        query_id: i64,
        repo_owner: String,
        repo_name: String,
        number: i64,
        highlight_since: Option<String>,
    },
    MarkEntryViewed {
        entry_id: i64,
        is_filter_stream: bool,
        viewed_at: String,
    },
    /// Enqueue all stale queries for background refresh, optionally skipping one
    /// (the query already being synced manually).
    EnqueueStale {
        skip_query_id: Option<i64>,
    },
    AddQuery {
        name: Option<String>,
        query: String,
    },
    AddFilterStream {
        parent_id: i64,
        kind: String,
        name: String,
        filter: String,
    },
    EditQuery {
        id: i64,
        name: Option<String>,
        query: String,
    },
    EditFilterStream {
        id: i64,
        name: String,
        filter: String,
    },
    DeleteQuery {
        query_id: i64,
    },
    DeleteFilterStream {
        id: i64,
    },
    SwapQueryPositions {
        upper_id: i64,
        lower_id: i64,
        active_id: i64,
    },
    SwapFilterStreamPositions {
        upper_id: i64,
        lower_id: i64,
        active_id: i64,
    },
    LoadComments {
        owner: String,
        repo: String,
        number: u64,
    },
    OpenBrowser {
        item: ItemEntry,
    },
    Comment {
        url: String,
        kind: String,
        body: String,
    },
    Approve {
        url: String,
        body: Option<String>,
    },
    Merge {
        url: String,
        strategy: MergeStrategy,
    },
    /// Persist that a single cached item has been read (viewed). Identified by the
    /// same unique key as the items table. Fire-and-forget; the front-end updates
    /// its in-memory copy and unread count itself.
    MarkItemRead {
        query_id: i64,
        repo_owner: String,
        repo_name: String,
        number: i64,
    },
    /// Mark every cached item of `query_id` read; when `filter` is Some, only items
    /// matching the (already `@me`-expanded) filter string. After updating the DB the
    /// query is reloaded so the front-end recomputes unread counts.
    MarkAllRead {
        query_id: i64,
        filter: Option<String>,
    },
}

/// Async engine shared by the TUI and GUI front-ends. Owns the background worker,
/// refresh timer, and command-handling loop; exposes a command channel in and an
/// `AppMessage` channel out.
pub struct Engine {
    cmd_tx: mpsc::Sender<EngineCommand>,
    msg_rx: mpsc::Receiver<AppMessage>,
}

impl Engine {
    /// Build the initial left-pane entries, resolve the current user, spawn the
    /// background worker / refresh timer / command loop, and return the engine
    /// handle plus the initial state.
    pub async fn start(pool: SqlitePool, gh: Octocrab) -> anyhow::Result<(Engine, EngineInit)> {
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

        let cu = github::get_current_user(&gh).await;
        let current_user = cu.as_ref().map(|u| u.login.clone());
        let current_user_name = cu.as_ref().and_then(|u| u.name.clone());
        let current_user_avatar_url = cu.as_ref().and_then(|u| u.avatar_url.clone());

        let (msg_tx, msg_rx) = mpsc::channel::<AppMessage>(32);
        let (sync_job_tx, sync_job_rx) = mpsc::channel::<SyncJob>(256);
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(64);

        // Spawn the sequential background sync worker.
        tokio::spawn(sync_worker_task(
            pool.clone(),
            gh.clone(),
            sync_job_rx,
            msg_tx.clone(),
        ));
        // Spawn the periodic refresh timer.
        tokio::spawn(refresh_timer_task(
            pool.clone(),
            sync_job_tx.clone(),
            msg_tx.clone(),
        ));
        // Spawn the command-handling loop.
        tokio::spawn(command_loop(pool, gh, cmd_rx, msg_tx, sync_job_tx));

        Ok((
            Engine { cmd_tx, msg_rx },
            EngineInit {
                entries,
                current_user,
                current_user_name,
                current_user_avatar_url,
            },
        ))
    }

    /// Send a command to the engine. Errors (channel closed) are ignored.
    pub async fn send(&self, cmd: EngineCommand) {
        let _ = self.cmd_tx.send(cmd).await;
    }

    /// A cloneable command sender for front-ends that issue commands from a
    /// non-async context (e.g. a GUI event handler can hold this and call
    /// `try_send` without `&self`/lifetime constraints).
    pub fn sender(&self) -> mpsc::Sender<EngineCommand> {
        self.cmd_tx.clone()
    }

    /// Await the next message from the engine (TUI/iced style).
    pub async fn recv(&mut self) -> Option<AppMessage> {
        self.msg_rx.recv().await
    }

    /// Non-blocking drain of the next message (GUI per-frame style).
    pub fn try_recv(&mut self) -> Option<AppMessage> {
        self.msg_rx.try_recv().ok()
    }
}

/// Dispatch `EngineCommand`s, spawning the underlying async tasks so the loop never
/// blocks on a single command (mirrors the previous in-`run_app` `tokio::spawn` use).
async fn command_loop(
    pool: SqlitePool,
    gh: Octocrab,
    mut cmd_rx: mpsc::Receiver<EngineCommand>,
    msg_tx: mpsc::Sender<AppMessage>,
    sync_tx: mpsc::Sender<SyncJob>,
) {
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            EngineCommand::LoadCached {
                query_id,
                highlight_since,
            } => {
                tokio::spawn(load_items_task(
                    pool.clone(),
                    query_id,
                    highlight_since,
                    msg_tx.clone(),
                ));
            }
            EngineCommand::Sync {
                query_id,
                query_str,
                highlight_since,
            } => {
                tokio::spawn(sync_task(
                    pool.clone(),
                    gh.clone(),
                    query_id,
                    query_str,
                    highlight_since,
                    msg_tx.clone(),
                ));
            }
            EngineCommand::SyncIfStale {
                query_id,
                query_str,
                highlight_since,
            } => {
                let pool2 = pool.clone();
                let gh2 = gh.clone();
                let tx2 = msg_tx.clone();
                tokio::spawn(async move {
                    if db::is_cache_stale(&pool2, query_id, CACHE_STALE_SECS)
                        .await
                        .unwrap_or(true)
                    {
                        let _ = tx2.send(AppMessage::SyncStarted { query_id }).await;
                        sync_task(pool2, gh2, query_id, query_str, highlight_since, tx2).await;
                    }
                });
            }
            EngineCommand::RefreshItem {
                query_id,
                repo_owner,
                repo_name,
                number,
                highlight_since,
            } => {
                // Re-fetch one item, upsert it into this query's cache, reload the
                // list, and report via Status (a light notice, not the sync spinner).
                let pool2 = pool.clone();
                let gh2 = gh.clone();
                let tx2 = msg_tx.clone();
                tokio::spawn(async move {
                    match github::fetch_item(&gh2, query_id, &repo_owner, &repo_name, number).await {
                        Ok(Some(item)) => {
                            if let Err(e) = db::upsert_item(&pool2, &item).await {
                                let _ =
                                    tx2.send(AppMessage::Status(format!("Refresh failed: {e}"))).await;
                                return;
                            }
                            load_items_task(pool2, query_id, highlight_since, tx2.clone()).await;
                            let _ = tx2.send(AppMessage::Status(format!("Refreshed #{number}"))).await;
                        }
                        Ok(None) => {
                            let _ = tx2
                                .send(AppMessage::Status(format!("#{number} no longer exists")))
                                .await;
                        }
                        Err(e) => {
                            let _ =
                                tx2.send(AppMessage::Status(format!("Refresh failed: {e}"))).await;
                        }
                    }
                });
            }
            EngineCommand::MarkEntryViewed {
                entry_id,
                is_filter_stream,
                viewed_at,
            } => {
                spawn_mark_entry_viewed(
                    pool.clone(),
                    entry_id,
                    is_filter_stream,
                    viewed_at,
                    msg_tx.clone(),
                );
            }
            EngineCommand::EnqueueStale { skip_query_id } => {
                let pool2 = pool.clone();
                let sync_tx2 = sync_tx.clone();
                let tx2 = msg_tx.clone();
                tokio::spawn(async move {
                    let queries = db::list_queries(&pool2).await.unwrap_or_default();
                    let mut count = 0usize;
                    for q in queries {
                        if Some(q.id) == skip_query_id {
                            continue;
                        }
                        if db::is_cache_stale(&pool2, q.id, CACHE_STALE_SECS)
                            .await
                            .unwrap_or(true)
                        {
                            let _ = sync_tx2
                                .send(SyncJob {
                                    query_id: q.id,
                                    query_str: q.query,
                                })
                                .await;
                            count += 1;
                        }
                    }
                    if count > 0 {
                        let _ = tx2.send(AppMessage::BgSyncQueued(count)).await;
                    }
                });
            }
            EngineCommand::AddQuery { name, query } => {
                let pool2 = pool.clone();
                let tx2 = msg_tx.clone();
                tokio::spawn(async move {
                    let name_opt = name.as_deref();
                    let label = name.clone().unwrap_or_else(|| query.clone());
                    match db::upsert_query(&pool2, &query, "pull_request", name_opt).await {
                        Ok(id) => {
                            let _ = tx2
                                .send(AppMessage::QueryAdded(QueryEntry {
                                    id,
                                    label,
                                    query_str: query,
                                    kind: "pull_request".into(),
                                    last_viewed_at: None,
                                }))
                                .await;
                        }
                        Err(e) => {
                            let _ = tx2
                                .send(AppMessage::Status(format!("save error: {e}")))
                                .await;
                        }
                    }
                });
            }
            EngineCommand::AddFilterStream {
                parent_id,
                kind,
                name,
                filter,
            } => {
                let pool2 = pool.clone();
                let tx2 = msg_tx.clone();
                tokio::spawn(async move {
                    match db::upsert_filter_stream(&pool2, parent_id, &name, &filter).await {
                        Ok(id) => {
                            let _ = tx2
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
                            let _ = tx2
                                .send(AppMessage::Status(format!(
                                    "save filter stream error: {e}"
                                )))
                                .await;
                        }
                    }
                });
            }
            EngineCommand::EditQuery { id, name, query } => {
                let pool2 = pool.clone();
                let tx2 = msg_tx.clone();
                tokio::spawn(async move {
                    match db::update_query(&pool2, id, name.as_deref(), &query).await {
                        Ok(()) => {
                            let _ = tx2
                                .send(AppMessage::QueryUpdated {
                                    id,
                                    new_name: name,
                                    new_query: query,
                                })
                                .await;
                        }
                        Err(e) => {
                            let _ = tx2
                                .send(AppMessage::Status(format!("edit query error: {e}")))
                                .await;
                        }
                    }
                });
            }
            EngineCommand::EditFilterStream { id, name, filter } => {
                let pool2 = pool.clone();
                let tx2 = msg_tx.clone();
                tokio::spawn(async move {
                    match db::update_filter_stream(&pool2, id, &name, &filter).await {
                        Ok(()) => {
                            let _ = tx2
                                .send(AppMessage::FilterStreamUpdated {
                                    id,
                                    new_name: name,
                                    new_filter: filter,
                                })
                                .await;
                        }
                        Err(e) => {
                            let _ = tx2
                                .send(AppMessage::Status(format!(
                                    "edit filter stream error: {e}"
                                )))
                                .await;
                        }
                    }
                });
            }
            EngineCommand::DeleteQuery { query_id } => {
                let pool2 = pool.clone();
                let tx2 = msg_tx.clone();
                tokio::spawn(async move {
                    if db::delete_query(&pool2, query_id).await.is_ok() {
                        let _ = tx2.send(AppMessage::QueryDeleted { query_id }).await;
                    }
                });
            }
            EngineCommand::DeleteFilterStream { id } => {
                let pool2 = pool.clone();
                let tx2 = msg_tx.clone();
                tokio::spawn(async move {
                    if db::delete_filter_stream(&pool2, id).await.is_ok() {
                        let _ = tx2.send(AppMessage::FilterStreamDeleted { id }).await;
                    }
                });
            }
            EngineCommand::SwapQueryPositions {
                upper_id,
                lower_id,
                active_id,
            } => {
                let pool2 = pool.clone();
                let tx2 = msg_tx.clone();
                tokio::spawn(async move {
                    if db::swap_query_positions(&pool2, upper_id, lower_id)
                        .await
                        .is_ok()
                    {
                        let _ = tx2
                            .send(AppMessage::QueriesSwapped {
                                upper_id,
                                lower_id,
                                active_id,
                            })
                            .await;
                    }
                });
            }
            EngineCommand::SwapFilterStreamPositions {
                upper_id,
                lower_id,
                active_id,
            } => {
                let pool2 = pool.clone();
                let tx2 = msg_tx.clone();
                tokio::spawn(async move {
                    if db::swap_filter_stream_positions(&pool2, upper_id, lower_id)
                        .await
                        .is_ok()
                    {
                        let _ = tx2
                            .send(AppMessage::FilterStreamsSwapped {
                                upper_id,
                                lower_id,
                                active_id,
                            })
                            .await;
                    }
                });
            }
            EngineCommand::LoadComments {
                owner,
                repo,
                number,
            } => {
                let gh2 = gh.clone();
                let tx2 = msg_tx.clone();
                tokio::spawn(async move {
                    match fetch_comments_task(&gh2, &owner, &repo, number).await {
                        Ok(comments) => {
                            let _ = tx2.send(AppMessage::CommentsLoaded(comments)).await;
                        }
                        Err(e) => {
                            let _ = tx2.send(AppMessage::CommentsFailed(e.to_string())).await;
                        }
                    }
                });
            }
            EngineCommand::OpenBrowser { item } => {
                let tx2 = msg_tx.clone();
                tokio::spawn(async move {
                    match execute_open_browser(&item).await {
                        Ok(msg) => {
                            let _ = tx2.send(AppMessage::ActionDone(msg)).await;
                        }
                        Err(e) => {
                            let _ = tx2.send(AppMessage::ActionError(e.to_string())).await;
                        }
                    }
                });
            }
            EngineCommand::Comment { url, kind, body } => {
                let tx2 = msg_tx.clone();
                tokio::spawn(async move {
                    match execute_comment(&url, &kind, &body).await {
                        Ok(msg) => {
                            let _ = tx2.send(AppMessage::ActionDone(msg)).await;
                        }
                        Err(e) => {
                            let _ = tx2.send(AppMessage::ActionError(e.to_string())).await;
                        }
                    }
                });
            }
            EngineCommand::Approve { url, body } => {
                let tx2 = msg_tx.clone();
                tokio::spawn(async move {
                    match execute_approve(&url, body.as_deref()).await {
                        Ok(msg) => {
                            let _ = tx2.send(AppMessage::ActionDone(msg)).await;
                        }
                        Err(e) => {
                            let _ = tx2.send(AppMessage::ActionError(e.to_string())).await;
                        }
                    }
                });
            }
            EngineCommand::Merge { url, strategy } => {
                let tx2 = msg_tx.clone();
                tokio::spawn(async move {
                    match execute_merge(&url, &strategy).await {
                        Ok(msg) => {
                            let _ = tx2.send(AppMessage::ActionDone(msg)).await;
                        }
                        Err(e) => {
                            let _ = tx2.send(AppMessage::ActionError(e.to_string())).await;
                        }
                    }
                });
            }
            EngineCommand::MarkItemRead {
                query_id,
                repo_owner,
                repo_name,
                number,
            } => {
                // Fire-and-forget persistence; the front-end already updated its
                // in-memory item + unread count.
                let pool2 = pool.clone();
                let tx2 = msg_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        db::mark_item_read(&pool2, query_id, &repo_owner, &repo_name, number).await
                    {
                        let _ = tx2
                            .send(AppMessage::Status(format!("mark read error: {e}")))
                            .await;
                    }
                });
            }
            EngineCommand::MarkAllRead { query_id, filter } => {
                // Update the DB, then reload the query so the front-end's
                // `ItemsLoaded` handler recomputes unread counts for this query's
                // entries (works whether or not the entry is currently selected).
                let pool2 = pool.clone();
                let tx2 = msg_tx.clone();
                tokio::spawn(async move {
                    let res = match &filter {
                        None => db::mark_all_items_read(&pool2, query_id).await,
                        Some(f) => mark_filtered_items_read(&pool2, query_id, f).await,
                    };
                    match res {
                        Ok(()) => load_items_task(pool2, query_id, None, tx2).await,
                        Err(e) => {
                            let _ = tx2
                                .send(AppMessage::Status(format!("mark all read error: {e}")))
                                .await;
                        }
                    }
                });
            }
        }
    }
}

/// Mark every cached item of `query_id` whose fields match `expanded_filter` read.
/// The filter is parsed application-side (`FilterQuery`) since it is not expressible
/// in SQL; already-read items are skipped.
async fn mark_filtered_items_read(
    pool: &SqlitePool,
    query_id: i64,
    expanded_filter: &str,
) -> anyhow::Result<()> {
    let fq = FilterQuery::parse(expanded_filter);
    for c in db::fetch_items(pool, query_id).await? {
        let item = cached_item_to_item_entry(c, None);
        if !item.read && fq.matches(&item) {
            db::mark_item_read(pool, query_id, &item.repo_owner, &item.repo_name, item.number)
                .await?;
        }
    }
    Ok(())
}
