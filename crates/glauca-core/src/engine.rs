// glauca-core::engine — framework 非依存の非同期エンジン処理。
// TUI/GUI 双方から利用する。$EDITOR 起動・端末制御などフロントエンド固有の処理は呼び出し側に残し、
// ここには pool/gh/mpsc チャネルだけで完結するタスクと、それらが受け渡すメッセージ型を集約する。

use crate::filter::StreamFilter;
use crate::logic::{cached_item_to_item_entry, is_item_unread};
use crate::types::{
    CommentEntry, FilterStreamEntry, ItemEntry, LeftPaneEntry, MergeStrategy, QueryEntry,
};
use crate::{db, github};
use chrono::Utc;
use octocrab::Octocrab;
use sqlx::SqlitePool;
use std::collections::HashSet;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Semaphore, mpsc};
use tracing::{debug, info, instrument, warn};

// ── Background messages ──────────────────────────────────────────────────────

/// Messages the engine emits to the front-end.
///
/// Serialized adjacently tagged (`{"type": "ItemsLoaded", "data": {…}}`) for the
/// web front-end (glauca-tauri), which forwards each one to JavaScript over the
/// Tauri event bus. The adjacent representation handles every variant shape
/// (struct/newtype/tuple/unit) uniformly. The TUI/GUI never serialize it.
#[derive(serde::Serialize)]
#[serde(tag = "type", content = "data")]
pub enum AppMessage {
    ItemsLoaded {
        query_id: i64,
        items: Vec<ItemEntry>,
        /// True when this load came from a background sync (the periodic worker),
        /// false for user-driven loads (select/manual sync/refresh/mark-read). The
        /// front-ends defer applying background updates to the currently-viewed
        /// query so the list doesn't change under the user.
        background: bool,
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
        /// True when the failure came from the background sync worker rather than
        /// a user-driven sync. Front-ends surface foreground errors prominently
        /// (e.g. a toast) but keep background failures quiet (status line only) so
        /// a persistent fault doesn't spam a notification every sync cycle.
        background: bool,
    },
    /// N background sync jobs were added to the worker queue.
    BgSyncQueued(usize),
    /// One background sync job finished (success, skip, or error) — the worker
    /// emits this regardless of outcome so the UI can decrement its counter.
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

/// query_id のうち、キュー投入済み or 同期実行中のもの。背景同期の重複投入を防ぐ。
type PendingSyncSet = Arc<Mutex<HashSet<i64>>>;

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

/// `gh` subcommand for an item kind: PRs use `pr`, everything else `issue`.
fn gh_subcommand(kind: &str) -> &'static str {
    if kind == "pull_request" {
        "pr"
    } else {
        "issue"
    }
}

/// Spawn a gh action future and report its outcome to the UI as `ActionDone` /
/// `ActionError`. Shared by the `OpenBrowser`/`Comment`/`Approve`/`Merge` arms.
fn spawn_action(
    tx: mpsc::Sender<AppMessage>,
    fut: impl std::future::Future<Output = anyhow::Result<String>> + Send + 'static,
) {
    tokio::spawn(async move {
        match fut.await {
            Ok(msg) => {
                let _ = tx.send(AppMessage::ActionDone(msg)).await;
            }
            Err(e) => {
                let _ = tx.send(AppMessage::ActionError(e.to_string())).await;
            }
        }
    });
}

pub async fn execute_open_browser(item: &ItemEntry) -> anyhow::Result<String> {
    let sub = gh_subcommand(&item.kind);
    let repo = item.repo_display();
    let mut cmd = tokio::process::Command::new("gh");
    cmd.args([sub, "view", "--web", &item.number.to_string(), "-R", &repo]);
    run_background_command(cmd, "Failed to open in browser").await?;
    Ok("Opened in browser".into())
}

/// Run a user-defined custom action against `item`. Each argv element (and each
/// env value) is rendered with `{{ key }}` placeholders from the item's context,
/// then the command is run directly (no shell) in the background — so `gh` and
/// user scripts inherit the environment and run as-is.
pub async fn execute_custom_action(
    action: &crate::actions::CustomAction,
    item: &ItemEntry,
) -> anyhow::Result<String> {
    let ctx = crate::actions::build_action_context(item);
    let argv = action
        .command
        .iter()
        .map(|t| crate::actions::render_template(t, &ctx))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("custom action '{}' has an empty command", action.name))?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    for (key, value) in &action.env {
        cmd.env(key, crate::actions::render_template(value, &ctx)?);
    }
    run_background_command(
        cmd,
        &format!("Custom action '{}' failed", action.display_label()),
    )
    .await?;
    Ok(format!("Ran: {}", action.display_label()))
}

pub async fn execute_comment(url: &str, kind: &str, body: &str) -> anyhow::Result<String> {
    let sub = gh_subcommand(kind);
    let mut cmd = tokio::process::Command::new("gh");
    cmd.args([sub, "comment", url, "--body", body]);
    run_background_command(cmd, &format!("gh {sub} comment failed")).await?;
    Ok("Comment posted".into())
}

/// A GitHub pull-request review event, submitted via `gh pr review`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewEvent {
    Comment,
    Approve,
    RequestChanges,
}

impl ReviewEvent {
    /// The `gh pr review` flag for this event.
    pub fn flag(&self) -> &'static str {
        match self {
            ReviewEvent::Comment => "--comment",
            ReviewEvent::Approve => "--approve",
            ReviewEvent::RequestChanges => "--request-changes",
        }
    }

    /// GitHub requires a body for comment / request-changes reviews; approve's is
    /// optional.
    pub fn requires_body(&self) -> bool {
        !matches!(self, ReviewEvent::Approve)
    }

    /// All events, in menu order (Approve first — the default for "Approve PR").
    pub fn all() -> Vec<Self> {
        vec![Self::Approve, Self::Comment, Self::RequestChanges]
    }

    /// Human label for selection menus.
    pub fn label(&self) -> &'static str {
        match self {
            ReviewEvent::Comment => "Comment",
            ReviewEvent::Approve => "Approve",
            ReviewEvent::RequestChanges => "Request changes",
        }
    }

    fn done_message(&self) -> &'static str {
        match self {
            ReviewEvent::Comment => "Review comment posted",
            ReviewEvent::Approve => "PR approved",
            ReviewEvent::RequestChanges => "Changes requested",
        }
    }
}

pub async fn execute_review(
    url: &str,
    event: ReviewEvent,
    body: Option<&str>,
) -> anyhow::Result<String> {
    let mut cmd = tokio::process::Command::new("gh");
    cmd.args(["pr", "review", event.flag(), url]);
    if let Some(b) = body {
        cmd.args(["-b", b]);
    }
    run_background_command(cmd, &format!("gh pr review {} failed", event.flag())).await?;
    Ok(event.done_message().into())
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
    background: bool,
    tx: mpsc::Sender<AppMessage>,
) {
    match db::fetch_items(&pool, query_id).await {
        Ok(cached) => {
            let items = cached.into_iter().map(cached_item_to_item_entry).collect();
            let _ = tx
                .send(AppMessage::ItemsLoaded {
                    query_id,
                    items,
                    background,
                })
                .await;
        }
        Err(e) => {
            let _ = tx
                .send(AppMessage::Status(format!("load error: {e}")))
                .await;
        }
    }
}

/// How a sync runs. `background` drives the front-end's deferred-refresh banner;
/// `incremental` narrows the fetch to items updated since the last fetch (a full
/// fetch when false, or when the query was never fetched).
#[derive(Clone, Copy)]
pub struct SyncOpts {
    pub background: bool,
    pub incremental: bool,
}

/// Overlap subtracted from `last_fetched_at` when building the incremental
/// `updated:>=` filter, to tolerate clock skew and updates made while the
/// previous fetch was in flight (re-fetching the overlap is harmless: upsert is
/// idempotent). Generous since the background interval is 5 min.
const INCREMENTAL_OVERLAP_SECS: i64 = 600;

/// GitHub Search returns at most this many results. A full fetch that hits the
/// cap may be truncated, so we must not prune in that case (we can't tell which
/// items truly fell out of the query vs. were cut off).
const SEARCH_RESULT_CAP: usize = 1000;

/// Fetch fresh results from GitHub API page by page, upserting each page immediately
/// so the UI can show results as they arrive rather than waiting for all pages.
#[allow(clippy::too_many_arguments)] // pool/gh/ids/opts/tx/gate are all genuinely needed
#[instrument(
    skip(pool, gh, query_str, opts, tx, gate),
    fields(background = opts.background, incremental = opts.incremental)
)]
pub async fn sync_task(
    pool: SqlitePool,
    gh: Octocrab,
    query_id: i64,
    query_str: String,
    opts: SyncOpts,
    tx: mpsc::Sender<AppMessage>,
    gate: RateLimitGate,
) {
    // Incremental fetches narrow the query to items updated since the last fetch.
    // `None` means a full fetch — used when never fetched, when forced
    // (`incremental == false`), or when reading the threshold fails (degrading to
    // a full fetch is safe, just more work).
    let since = if opts.incremental {
        db::updated_since(&pool, query_id, INCREMENTAL_OVERLAP_SECS)
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    // A full fetch (no `since`) is the authoritative result set, so we collect
    // its keys to prune items that no longer match the query. Skipped for
    // incremental fetches (a partial set can't tell us what fell out).
    let is_full = since.is_none();
    let mut keep_keys: Vec<(String, String, i64)> = Vec::new();

    // Reload the query's items from the DB and push them to the UI. Called after
    // each page (incremental display) and after a prune actually removes rows.
    let reload = || load_items_task(pool.clone(), query_id, opts.background, tx.clone());

    let mut after: Option<String> = None;
    let mut total_count = 0usize;

    loop {
        let result = github::search_page(
            &gh,
            query_id,
            &query_str,
            since.as_deref(),
            after.as_deref(),
        )
        .await;
        match result {
            Err(github::SearchError::RateLimited) => {
                // Pause background sync until the limit resets; surface a notice
                // rather than a hard error so the UI doesn't look broken.
                let now = Utc::now().timestamp();
                let until = backoff_until(&gh, now).await;
                gate.block_until(until);
                warn!(
                    until,
                    wait_secs = (until - now).max(0),
                    "rate limited; pausing background sync"
                );
                let _ = tx
                    .send(AppMessage::Status(format!(
                        "GitHub rate limited; auto-sync paused ~{}s",
                        (until - now).max(0)
                    )))
                    .await;
                return;
            }
            Err(github::SearchError::Other(e)) => {
                warn!(error = %e, "sync failed");
                let _ = tx
                    .send(AppMessage::SyncError {
                        query_id,
                        error: format!("GitHub API error: {e}"),
                        background: opts.background,
                    })
                    .await;
                return;
            }
            Ok(page) => {
                let has_next = page.has_next_page;
                let cursor = page.end_cursor.clone();

                // Upsert this page's items into SQLite.
                for item in &page.items {
                    if is_full {
                        keep_keys.push((
                            item.repo_owner.clone(),
                            item.repo_name.clone(),
                            item.number,
                        ));
                    }
                    if let Err(e) = db::upsert_item(&pool, item).await {
                        let _ = tx
                            .send(AppMessage::SyncError {
                                query_id,
                                error: format!("db write error: {e}"),
                                background: opts.background,
                            })
                            .await;
                        return;
                    }
                }
                total_count += page.items.len();
                debug!(page_items = page.items.len(), total_count, "fetched page");

                // Reload from DB after each page so the UI shows results immediately.
                reload().await;

                // Stop when GitHub reports no further pages, or defensively if
                // it claims another page but hands back no cursor to fetch it.
                if !has_next {
                    break;
                }
                let Some(cursor) = cursor else {
                    break;
                };
                after = Some(cursor);
            }
        }
    }

    // After an untruncated full fetch, drop cached items the query no longer
    // returns (e.g. a PR that was merged and left an `is:open` query). Skipped
    // when the result hit the cap, since the set may be truncated.
    if is_full && total_count < SEARCH_RESULT_CAP {
        match db::prune_query_items(&pool, query_id, &keep_keys).await {
            // Only reload when rows were actually removed — the final per-page
            // reload already reflects every upsert.
            Ok(deleted) if deleted > 0 => {
                debug!(deleted, "pruned items no longer matching query");
                reload().await;
            }
            Ok(_) => {}
            Err(e) => {
                let _ = tx
                    .send(AppMessage::Status(format!("prune error: {e}")))
                    .await;
            }
        }
    }

    // Mark the query as freshly fetched only after all pages are done.
    if let Err(e) = db::mark_fetched(&pool, query_id).await {
        let _ = tx
            .send(AppMessage::SyncError {
                query_id,
                error: format!("mark fetched error: {e}"),
                background: opts.background,
            })
            .await;
        return;
    }
    info!(total_count, "sync done");
    let _ = tx
        .send(AppMessage::SyncDone {
            query_id,
            count: total_count,
        })
        .await;
}

// ── Background sync worker & refresh timer ────────────────────────────────────

/// Default background auto-refresh interval / cache-staleness threshold, used
/// when the front-end's settings file doesn't specify one.
pub const DEFAULT_SYNC_INTERVAL_SECS: u64 = 60;
/// Floor for the configured interval: avoids hammering the GitHub API and a
/// zero-duration `tokio::time::interval` (which panics).
pub const MIN_SYNC_INTERVAL_SECS: u64 = 10;
/// Fallback backoff when a rate limit is hit but no reset time is available
/// (e.g. a secondary/abuse limit, where `/rate_limit` still shows remaining>0).
pub const DEFAULT_RATELIMIT_BACKOFF_SECS: i64 = 60;

/// Default age (days) past which a cached item's re-fetchable `body` is cleared to
/// reclaim space; also clears terminal-state items regardless of age. Used when the
/// front-end's settings file doesn't specify one. See `db::clear_stale_bodies`.
pub const DEFAULT_RETENTION_DAYS: u64 = 90;
/// Default per-query cap on cached rows; read overflow beyond this (oldest first)
/// is pruned to bound row growth. See `db::prune_query_overflow`.
pub const DEFAULT_MAX_ITEMS_PER_QUERY: u64 = 1500;
/// How often the local cache-maintenance pass runs. Purely local (no GitHub API),
/// so it doesn't consume quota; it also runs once shortly after startup so short
/// sessions still get maintained.
pub const MAINTENANCE_INTERVAL_SECS: u64 = 6 * 60 * 60;
/// Delay before the first maintenance sweep, so the startup burst of syncs settles
/// before the sweep's `VACUUM` takes an exclusive lock.
pub const MAINTENANCE_STARTUP_DELAY_SECS: u64 = 60;
/// Max concurrent single-item re-fetches (`RefreshItem`). Bounds the burst of
/// GitHub requests when a front-end auto-re-fetches maintenance-cleared bodies
/// while the user scrolls through cleared items.
pub const MAX_CONCURRENT_ITEM_REFRESH: usize = 4;

/// Tunables for the background cache-maintenance pass. Sized generously by default
/// because clearing `body` is non-destructive (re-fetched on open) and overflow
/// pruning never touches unread rows.
#[derive(Debug, Clone, Copy)]
pub struct MaintenanceConfig {
    /// Age (days) past which an item's `body` is cleared. Terminal-state items are
    /// cleared regardless of age.
    pub retention_days: u64,
    /// Per-query cap on cached rows; read overflow beyond it is pruned.
    pub max_items_per_query: u64,
}

/// Clamp a configured interval to at least `MIN_SYNC_INTERVAL_SECS`.
pub fn effective_interval(secs: u64) -> u64 {
    secs.max(MIN_SYNC_INTERVAL_SECS)
}

/// Shared, cloneable gate that pauses background sync after a rate limit is hit.
/// Holds the unix epoch (seconds) until which background work should be skipped;
/// `0` means open (not limited).
#[derive(Clone, Default)]
pub struct RateLimitGate {
    until: Arc<AtomicI64>,
}

impl RateLimitGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when background sync may proceed (not currently rate-limited).
    pub fn is_open(&self, now: i64) -> bool {
        now >= self.until.load(Ordering::Relaxed)
    }

    /// Pause background sync until `epoch` (unix seconds).
    pub fn block_until(&self, epoch: i64) {
        self.until.store(epoch, Ordering::Relaxed);
    }

    /// Seconds remaining until the gate reopens (0 when already open).
    pub fn remaining_secs(&self, now: i64) -> i64 {
        (self.until.load(Ordering::Relaxed) - now).max(0)
    }
}

/// Resolve the unix epoch to back off until after a rate limit. Reads the free
/// REST `/rate_limit` endpoint (no quota cost): if the GraphQL resource is
/// exhausted, use its `reset`; otherwise (a secondary/abuse limit) fall back to
/// a fixed cooldown. Any failure also falls back to the fixed cooldown.
async fn backoff_until(gh: &Octocrab, now: i64) -> i64 {
    // Used when there's no usable reset time: a secondary/abuse limit (which
    // leaves the GraphQL quota with remaining>0) or a failed `/rate_limit` read.
    let fixed_cooldown = now + DEFAULT_RATELIMIT_BACKOFF_SECS;
    match gh.ratelimit().get().await {
        Ok(rl) => {
            let graphql = rl.resources.graphql.unwrap_or(rl.rate);
            if graphql.remaining == 0 {
                info!(
                    reset = graphql.reset,
                    "rate limit: waiting for graphql reset"
                );
                graphql.reset as i64
            } else {
                info!("rate limit: secondary/abuse, using fixed cooldown");
                fixed_cooldown
            }
        }
        Err(e) => {
            warn!(error = %e, "rate_limit query failed; using fixed cooldown");
            fixed_cooldown
        }
    }
}

/// Processes `SyncJob`s sequentially, skipping jobs whose cache is already fresh.
/// Sends `BgSyncJobDone` after each job (regardless of outcome) so the UI counter
/// stays accurate.
pub async fn sync_worker_task(
    pool: SqlitePool,
    gh: Octocrab,
    mut rx: mpsc::Receiver<SyncJob>,
    tx: mpsc::Sender<AppMessage>,
    stale_secs: i64,
    gate: RateLimitGate,
    pending: PendingSyncSet,
) {
    while let Some(job) = rx.recv().await {
        // Skip (don't hit the API) while rate-limited; the query stays stale and
        // will be re-enqueued once the gate reopens.
        if !gate.is_open(Utc::now().timestamp()) {
            debug!(query_id = job.query_id, "skip sync: rate-limited");
            pending.lock().unwrap().remove(&job.query_id);
            let _ = tx.send(AppMessage::BgSyncJobDone).await;
            continue;
        }
        let stale = db::is_cache_stale(&pool, job.query_id, stale_secs)
            .await
            .unwrap_or(true);
        if stale {
            sync_task(
                pool.clone(),
                gh.clone(),
                job.query_id,
                job.query_str,
                SyncOpts {
                    background: true,
                    incremental: true,
                },
                tx.clone(),
                gate.clone(),
            )
            .await;
        }
        // Job done (synced, skipped-as-fresh, or failed): release the coalescing
        // slot so the next timer tick can re-enqueue this query.
        pending.lock().unwrap().remove(&job.query_id);
        let _ = tx.send(AppMessage::BgSyncJobDone).await;
    }
}

/// Enqueue every stale query (optionally skipping one) to the sync worker and
/// report how many were queued. Shared by the refresh timer and the
/// `EnqueueStale` command.
async fn enqueue_stale_queries(
    pool: &SqlitePool,
    sync_tx: &mpsc::Sender<SyncJob>,
    app_tx: &mpsc::Sender<AppMessage>,
    skip_query_id: Option<i64>,
    stale_secs: i64,
    gate: &RateLimitGate,
    pending: &PendingSyncSet,
) {
    // Don't fill the queue while rate-limited; the timer will try again later.
    if !gate.is_open(Utc::now().timestamp()) {
        debug!("skip enqueue: rate-limited");
        return;
    }
    let queries = db::list_queries(pool).await.unwrap_or_default();
    let mut count = 0usize;
    for q in queries {
        if Some(q.id) == skip_query_id {
            continue;
        }
        if db::is_cache_stale(pool, q.id, stale_secs)
            .await
            .unwrap_or(true)
        {
            // Coalesce: enqueue only if this query isn't already queued or in
            // flight, so a long offline stretch can't pile up duplicate jobs.
            // Lock is dropped before the await below (std Mutex must not be held
            // across `.await`).
            let newly = pending.lock().unwrap().insert(q.id);
            if newly {
                let _ = sync_tx
                    .send(SyncJob {
                        query_id: q.id,
                        query_str: q.query,
                    })
                    .await;
                count += 1;
            }
        }
    }
    debug!(count, "enqueued stale queries");
    if count > 0 {
        let _ = app_tx.send(AppMessage::BgSyncQueued(count)).await;
    }
}

/// Every `interval_secs`, enqueues all stale queries to the worker. The same
/// value is used as the staleness threshold, so a query syncs roughly every
/// `interval_secs`.
pub async fn refresh_timer_task(
    pool: SqlitePool,
    sync_tx: mpsc::Sender<SyncJob>,
    app_tx: mpsc::Sender<AppMessage>,
    interval_secs: u64,
    gate: RateLimitGate,
    pending: PendingSyncSet,
) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));
    interval.tick().await; // skip the immediate first tick
    loop {
        interval.tick().await;
        enqueue_stale_queries(
            &pool,
            &sync_tx,
            &app_tx,
            None,
            interval_secs as i64,
            &gate,
            &pending,
        )
        .await;
    }
}

/// One local cache-maintenance sweep: clear stale `body` blobs, prune each query's
/// read overflow, then `VACUUM` to return the freed pages to the OS. All local
/// (no GitHub API). Errors are logged and skipped so one failing step can't abort
/// the rest or crash the task.
async fn run_maintenance(pool: &SqlitePool, cfg: MaintenanceConfig) {
    // Clamp to sane floors: `retention_days = 0` would clear essentially every
    // body, and `max_items_per_query = 0` would make the overflow subquery
    // `LIMIT 0` and delete *all* read rows. Mirrors `effective_interval`.
    let retention_days = cfg.retention_days.max(1) as i64;
    let max_items = cfg.max_items_per_query.max(1) as i64;
    match db::clear_stale_bodies(pool, retention_days).await {
        Ok(n) => info!(cleared = n, "maintenance: cleared stale item bodies"),
        Err(e) => warn!(error = %e, "maintenance: clear_stale_bodies failed"),
    }
    let queries = match db::list_queries(pool).await {
        Ok(queries) => queries,
        Err(e) => {
            warn!(error = %e, "maintenance: list_queries failed");
            Vec::new()
        }
    };
    for q in queries {
        match db::prune_query_overflow(pool, q.id, max_items).await {
            Ok(n) if n > 0 => info!(query_id = q.id, deleted = n, "maintenance: pruned overflow"),
            Ok(_) => {}
            Err(e) => warn!(query_id = q.id, error = %e, "maintenance: prune failed"),
        }
    }
    match db::vacuum(pool).await {
        Ok(true) => info!("maintenance: vacuumed"),
        Ok(false) => {}
        Err(e) => warn!(error = %e, "maintenance: vacuum failed"),
    }
}

/// Runs `run_maintenance` shortly after startup, then every
/// `MAINTENANCE_INTERVAL_SECS`. The startup delay lets the initial burst of syncs
/// settle before the first sweep, so its `VACUUM` (which needs exclusive access)
/// doesn't contend with the initial `upsert_item` writes.
pub async fn maintenance_task(pool: SqlitePool, cfg: MaintenanceConfig) {
    tokio::time::sleep(tokio::time::Duration::from_secs(
        MAINTENANCE_STARTUP_DELAY_SECS,
    ))
    .await;
    let mut interval =
        tokio::time::interval(tokio::time::Duration::from_secs(MAINTENANCE_INTERVAL_SECS));
    loop {
        interval.tick().await; // first tick fires immediately (post-delay)
        run_maintenance(&pool, cfg).await;
    }
}

// ── Engine: command-driven async facade ───────────────────────────────────────

/// Initial state produced by `Engine::start`: the left-pane entries (root queries
/// interleaved with their filter streams) and the authenticated user login.
#[derive(serde::Serialize)]
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
    },
    /// Unconditional GitHub sync for a root query (incremental: only items
    /// updated since the last fetch).
    Sync {
        query_id: i64,
        query_str: String,
    },
    /// Forced full re-fetch of a root query, ignoring `last_fetched_at`. Re-pages
    /// the whole result set and prunes cached items that no longer match.
    FullResync {
        query_id: i64,
        query_str: String,
    },
    /// GitHub sync only if the query's cache is stale (sends `SyncStarted` if it runs).
    SyncIfStale {
        query_id: i64,
        query_str: String,
    },
    /// Re-fetch a single item from GitHub and upsert it into `query_id`'s cache,
    /// then reload that query's items. Used by the per-item refresh action.
    RefreshItem {
        query_id: i64,
        repo_owner: String,
        repo_name: String,
        number: i64,
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
        item: Box<ItemEntry>,
    },
    /// Run a user-defined custom action against an item (see `actions` module).
    RunCustomAction {
        action: Box<crate::actions::CustomAction>,
        item: Box<ItemEntry>,
    },
    Comment {
        url: String,
        kind: String,
        body: String,
    },
    SubmitReview {
        url: String,
        event: ReviewEvent,
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

/// Build the left-pane entries from the DB: root queries in position order, each
/// followed by its filter streams. This is the single source of the left-pane
/// ordering — `Engine::start` uses it for the initial state and front-ends
/// (glauca-tauri's `list_entries`) reuse it to rebuild the pane after structural
/// changes, so the interleaving logic is never re-implemented per front-end.
///
/// A DB read failure here is propagated, not swallowed — so `Engine::start`
/// aborts launch rather than starting with an empty left pane. This is
/// deliberate: the reads run against a freshly-opened, freshly-migrated pool, so
/// a failure means something is genuinely wrong (corruption, disk error, schema
/// mismatch), and showing an empty pane would look like the user's saved queries
/// silently vanished — worse than failing loudly.
pub async fn load_left_pane_entries(pool: &SqlitePool) -> anyhow::Result<Vec<LeftPaneEntry>> {
    let query_rows = db::list_queries(pool).await?;
    let mut entries: Vec<LeftPaneEntry> = Vec::new();
    for r in query_rows {
        let streams = db::list_filter_streams(pool, r.id).await?;
        let kind = r.kind.clone();
        let label = r.name.clone().unwrap_or_else(|| r.query.clone());
        entries.push(LeftPaneEntry::Query(QueryEntry {
            id: r.id,
            label,
            query_str: r.query.clone(),
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
    Ok(entries)
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
    pub async fn start(
        pool: SqlitePool,
        gh: Octocrab,
        sync_interval_secs: u64,
        maintenance: MaintenanceConfig,
    ) -> anyhow::Result<(Engine, EngineInit)> {
        let entries = load_left_pane_entries(&pool).await?;

        let cu = github::get_current_user(&gh).await;
        let current_user = cu.as_ref().map(|u| u.login.clone());
        let current_user_name = cu.as_ref().and_then(|u| u.name.clone());
        let current_user_avatar_url = cu.as_ref().and_then(|u| u.avatar_url.clone());

        let (msg_tx, msg_rx) = mpsc::channel::<AppMessage>(32);
        let (sync_job_tx, sync_job_rx) = mpsc::channel::<SyncJob>(256);
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(64);

        // One interval drives both the timer tick and the staleness threshold, so
        // a query syncs roughly every `interval` seconds.
        let interval = effective_interval(sync_interval_secs);
        let stale = interval as i64;
        info!(sync_interval_secs = interval, "engine started");
        // Shared gate that pauses background sync after a rate limit is hit.
        let gate = RateLimitGate::new();
        // Shared set coalescing background sync jobs: at most one queued/in-flight
        // job per query, so a long offline stretch can't pile up duplicates.
        let pending: PendingSyncSet = Arc::new(Mutex::new(HashSet::new()));

        // Spawn the sequential background sync worker.
        tokio::spawn(sync_worker_task(
            pool.clone(),
            gh.clone(),
            sync_job_rx,
            msg_tx.clone(),
            stale,
            gate.clone(),
            pending.clone(),
        ));
        // Spawn the periodic refresh timer.
        tokio::spawn(refresh_timer_task(
            pool.clone(),
            sync_job_tx.clone(),
            msg_tx.clone(),
            interval,
            gate.clone(),
            pending.clone(),
        ));
        // Spawn the periodic local cache-maintenance sweep (body clears, overflow
        // prune, VACUUM). Purely local, so it ignores the rate-limit gate.
        tokio::spawn(maintenance_task(pool.clone(), maintenance));
        // Spawn the command-handling loop.
        tokio::spawn(command_loop(
            pool,
            gh,
            cmd_rx,
            msg_tx,
            sync_job_tx,
            stale,
            gate,
            pending,
        ));

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

    /// Non-blocking drain of the next message, for batching after an awaited
    /// `recv` (the GUI applies a whole burst in one frame).
    pub fn try_recv(&mut self) -> Option<AppMessage> {
        self.msg_rx.try_recv().ok()
    }
}

/// Dispatch `EngineCommand`s, spawning the underlying async tasks so the loop never
/// blocks on a single command (mirrors the previous in-`run_app` `tokio::spawn` use).
#[allow(clippy::too_many_arguments)] // pool/gh/channels/stale/gate/pending are all genuinely needed
async fn command_loop(
    pool: SqlitePool,
    gh: Octocrab,
    mut cmd_rx: mpsc::Receiver<EngineCommand>,
    msg_tx: mpsc::Sender<AppMessage>,
    sync_tx: mpsc::Sender<SyncJob>,
    stale_secs: i64,
    gate: RateLimitGate,
    pending: PendingSyncSet,
) {
    // Bounds concurrent single-item re-fetches (see the RefreshItem arm). Shared
    // across all spawned RefreshItem tasks for the loop's lifetime.
    let item_refresh_sem = Arc::new(Semaphore::new(MAX_CONCURRENT_ITEM_REFRESH));
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            EngineCommand::LoadCached { query_id } => {
                tokio::spawn(load_items_task(
                    pool.clone(),
                    query_id,
                    false, // user-driven load
                    msg_tx.clone(),
                ));
            }
            EngineCommand::Sync {
                query_id,
                query_str,
            } => {
                tokio::spawn(sync_task(
                    pool.clone(),
                    gh.clone(),
                    query_id,
                    query_str,
                    SyncOpts {
                        background: false,
                        incremental: true,
                    },
                    msg_tx.clone(),
                    gate.clone(),
                ));
            }
            EngineCommand::FullResync {
                query_id,
                query_str,
            } => {
                // Forced full fetch (incremental: false → no `updated:>=` filter),
                // which re-pages the whole result set and prunes items that no
                // longer match. Foreground so it applies live.
                tokio::spawn(sync_task(
                    pool.clone(),
                    gh.clone(),
                    query_id,
                    query_str,
                    SyncOpts {
                        background: false,
                        incremental: false,
                    },
                    msg_tx.clone(),
                    gate.clone(),
                ));
            }
            EngineCommand::SyncIfStale {
                query_id,
                query_str,
            } => {
                let pool2 = pool.clone();
                let gh2 = gh.clone();
                let tx2 = msg_tx.clone();
                let gate2 = gate.clone();
                tokio::spawn(async move {
                    if db::is_cache_stale(&pool2, query_id, stale_secs)
                        .await
                        .unwrap_or(true)
                    {
                        let _ = tx2.send(AppMessage::SyncStarted { query_id }).await;
                        sync_task(
                            pool2,
                            gh2,
                            query_id,
                            query_str,
                            SyncOpts {
                                background: false,
                                incremental: true,
                            },
                            tx2,
                            gate2,
                        )
                        .await;
                    }
                });
            }
            EngineCommand::RefreshItem {
                query_id,
                repo_owner,
                repo_name,
                number,
            } => {
                // Re-fetch one item, upsert it into this query's cache, reload the
                // list, and report via Status (a light notice, not the sync spinner).
                let pool2 = pool.clone();
                let gh2 = gh.clone();
                let tx2 = msg_tx.clone();
                // Cap concurrency: front-ends fire this automatically to re-fetch a
                // maintenance-cleared body when an item is viewed, so scrolling
                // quickly through a backlog of cleared items could otherwise spawn a
                // burst of concurrent GitHub requests and trip a secondary rate
                // limit. The permit is held for the whole fetch.
                let sem = item_refresh_sem.clone();
                tokio::spawn(async move {
                    let _permit = sem.acquire_owned().await.ok();
                    match github::fetch_item(&gh2, query_id, &repo_owner, &repo_name, number).await
                    {
                        Ok(Some(item)) => {
                            if let Err(e) = db::upsert_item(&pool2, &item).await {
                                let _ = tx2
                                    .send(AppMessage::Status(format!("Refresh failed: {e}")))
                                    .await;
                                return;
                            }
                            load_items_task(pool2, query_id, false, tx2.clone()).await;
                            let _ = tx2
                                .send(AppMessage::Status(format!("Refreshed #{number}")))
                                .await;
                        }
                        Ok(None) => {
                            let _ = tx2
                                .send(AppMessage::Status(format!("#{number} no longer exists")))
                                .await;
                        }
                        Err(e) => {
                            let _ = tx2
                                .send(AppMessage::Status(format!("Refresh failed: {e}")))
                                .await;
                        }
                    }
                });
            }
            EngineCommand::EnqueueStale { skip_query_id } => {
                let pool2 = pool.clone();
                let sync_tx2 = sync_tx.clone();
                let tx2 = msg_tx.clone();
                let gate2 = gate.clone();
                let pending2 = pending.clone();
                tokio::spawn(async move {
                    enqueue_stale_queries(
                        &pool2,
                        &sync_tx2,
                        &tx2,
                        skip_query_id,
                        stale_secs,
                        &gate2,
                        &pending2,
                    )
                    .await;
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
                                }))
                                .await;
                        }
                        Err(e) => {
                            let _ = tx2
                                .send(AppMessage::Status(format!("save filter stream error: {e}")))
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
                                .send(AppMessage::Status(format!("edit filter stream error: {e}")))
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
                spawn_action(
                    msg_tx.clone(),
                    async move { execute_open_browser(&item).await },
                );
            }
            EngineCommand::RunCustomAction { action, item } => {
                spawn_action(msg_tx.clone(), async move {
                    execute_custom_action(&action, &item).await
                });
            }
            EngineCommand::Comment { url, kind, body } => {
                spawn_action(msg_tx.clone(), async move {
                    execute_comment(&url, &kind, &body).await
                });
            }
            EngineCommand::SubmitReview { url, event, body } => {
                spawn_action(msg_tx.clone(), async move {
                    execute_review(&url, event, body.as_deref()).await
                });
            }
            EngineCommand::Merge { url, strategy } => {
                spawn_action(msg_tx.clone(), async move {
                    execute_merge(&url, &strategy).await
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
                        Ok(()) => load_items_task(pool2, query_id, false, tx2).await,
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
/// The filter is parsed application-side (`StreamFilter`) since it is not expressible
/// in SQL; already-read items are skipped. `expanded_filter` already has `@me`
/// substituted, so it is parsed without a second expansion.
async fn mark_filtered_items_read(
    pool: &SqlitePool,
    query_id: i64,
    expanded_filter: &str,
) -> anyhow::Result<()> {
    let fq = StreamFilter::parse_expanded(expanded_filter);
    for c in db::fetch_items(pool, query_id).await? {
        let item = cached_item_to_item_entry(c);
        if is_item_unread(&item.updated_at, item.last_read_updated_at.as_deref())
            && fq.matches(&item)
        {
            db::mark_item_read(
                pool,
                query_id,
                &item.repo_owner,
                &item.repo_name,
                item.number,
            )
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_interval_clamps_to_floor() {
        assert_eq!(effective_interval(0), MIN_SYNC_INTERVAL_SECS);
        assert_eq!(effective_interval(5), MIN_SYNC_INTERVAL_SECS);
        assert_eq!(effective_interval(60), 60);
        assert_eq!(effective_interval(120), 120);
    }

    fn custom_action(command: Vec<&str>) -> crate::actions::CustomAction {
        crate::actions::CustomAction {
            name: "test".into(),
            label: None,
            command: command.into_iter().map(String::from).collect(),
            kinds: vec![],
            env: Default::default(),
        }
    }

    #[tokio::test]
    async fn custom_action_substitutes_into_argv() {
        // Renders to `test 5 -eq 5` → exit 0. If substitution failed the shell
        // expression would be malformed and exit non-zero.
        let item = ItemEntry {
            number: 5,
            ..Default::default()
        };
        let action = custom_action(vec!["sh", "-c", "test {{ number }} -eq 5"]);
        let msg = execute_custom_action(&action, &item).await.unwrap();
        assert!(msg.contains("test"), "unexpected message: {msg}");
    }

    #[tokio::test]
    async fn custom_action_empty_command_errors() {
        let action = custom_action(vec![]);
        assert!(
            execute_custom_action(&action, &ItemEntry::default())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn custom_action_nonzero_exit_errors() {
        let action = custom_action(vec!["sh", "-c", "exit 1"]);
        assert!(
            execute_custom_action(&action, &ItemEntry::default())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn custom_action_unknown_variable_errors() {
        let action = custom_action(vec!["echo", "{{ bogus }}"]);
        assert!(
            execute_custom_action(&action, &ItemEntry::default())
                .await
                .is_err()
        );
    }

    #[test]
    fn rate_limit_gate_opens_and_blocks() {
        let gate = RateLimitGate::new();
        // A fresh gate is open and has no wait.
        assert!(gate.is_open(1000));
        assert_eq!(gate.remaining_secs(1000), 0);

        gate.block_until(1100);
        assert!(!gate.is_open(1000));
        assert_eq!(gate.remaining_secs(1000), 100);
        // Exactly at the reset it reopens.
        assert!(gate.is_open(1100));
        assert_eq!(gate.remaining_secs(1100), 0);
        // Past the reset it stays open with no negative wait.
        assert!(gate.is_open(1200));
        assert_eq!(gate.remaining_secs(1200), 0);
    }

    /// While the worker isn't draining (e.g. offline, jobs stuck in flight),
    /// repeated timer ticks must not pile up duplicate jobs for the same query:
    /// each stale query is enqueued at most once until its slot is released.
    #[tokio::test]
    async fn enqueue_stale_coalesces_duplicate_jobs() {
        use tempfile::NamedTempFile;

        let file = NamedTempFile::new().expect("tempfile");
        let pool = db::open_pool(&file.path().to_path_buf())
            .await
            .expect("open pool");

        // Freshly-created queries have last_fetched_at = NULL, so both are stale.
        let q1 = db::upsert_query(&pool, "repo:o/a is:pr", "pull_request", None)
            .await
            .expect("q1");
        let q2 = db::upsert_query(&pool, "repo:o/b is:pr", "pull_request", None)
            .await
            .expect("q2");

        let (sync_tx, mut sync_rx) = mpsc::channel::<SyncJob>(256);
        let (app_tx, _app_rx) = mpsc::channel::<AppMessage>(256);
        let gate = RateLimitGate::new();
        let pending: PendingSyncSet = Arc::new(Mutex::new(HashSet::new()));

        // First pass enqueues both stale queries.
        enqueue_stale_queries(&pool, &sync_tx, &app_tx, None, 60, &gate, &pending).await;
        // Second pass: nothing drained the queue, so both stay pending and must
        // NOT be enqueued again (without coalescing this would send 4 total).
        enqueue_stale_queries(&pool, &sync_tx, &app_tx, None, 60, &gate, &pending).await;

        drop(sync_tx); // close the channel so the drain below terminates
        let mut ids = Vec::new();
        while let Some(job) = sync_rx.recv().await {
            ids.push(job.query_id);
        }
        ids.sort();
        assert_eq!(ids, vec![q1, q2], "each stale query enqueued exactly once");
    }

    /// The other half of coalescing: once the worker releases a query's slot,
    /// a still-stale query must be re-enqueued on the next pass. This is what
    /// keeps an offline query retrying roughly once per interval rather than
    /// getting stuck forever after its first (failed) attempt.
    #[tokio::test]
    async fn enqueue_stale_reenqueues_after_slot_released() {
        use tempfile::NamedTempFile;

        let file = NamedTempFile::new().expect("tempfile");
        let pool = db::open_pool(&file.path().to_path_buf())
            .await
            .expect("open pool");
        let q1 = db::upsert_query(&pool, "repo:o/a is:pr", "pull_request", None)
            .await
            .expect("q1");

        let (sync_tx, mut sync_rx) = mpsc::channel::<SyncJob>(256);
        let (app_tx, _app_rx) = mpsc::channel::<AppMessage>(256);
        let gate = RateLimitGate::new();
        let pending: PendingSyncSet = Arc::new(Mutex::new(HashSet::new()));

        // First pass enqueues the stale query.
        enqueue_stale_queries(&pool, &sync_tx, &app_tx, None, 60, &gate, &pending).await;
        let first = sync_rx.recv().await.expect("first job");
        assert_eq!(first.query_id, q1);

        // Slot still held (worker hasn't finished): the still-stale query must
        // NOT be re-enqueued.
        enqueue_stale_queries(&pool, &sync_tx, &app_tx, None, 60, &gate, &pending).await;
        assert!(
            sync_rx.try_recv().is_err(),
            "must not re-enqueue while the slot is held"
        );

        // Worker completion releases the slot; the still-stale query re-enqueues.
        pending.lock().unwrap().remove(&q1);
        enqueue_stale_queries(&pool, &sync_tx, &app_tx, None, 60, &gate, &pending).await;
        let second = sync_rx.recv().await.expect("re-enqueued job");
        assert_eq!(second.query_id, q1, "released slot allows re-enqueue");
    }
}
