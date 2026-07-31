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

/// Coalesces background sync jobs: tracks which queries already have a queued or
/// in-flight job so a long offline stretch can't pile up duplicate jobs. A thin
/// newtype over shared state, mirroring `RateLimitGate`.
#[derive(Clone, Default)]
pub struct SyncCoalescer {
    /// query_ids that are queued or in flight.
    pending: Arc<Mutex<HashSet<i64>>>,
}

impl SyncCoalescer {
    fn new() -> Self {
        Self::default()
    }

    /// True if this query already has a queued or in-flight job. Lets callers
    /// skip expensive staleness checks for queries that are already pending.
    fn is_pending(&self, query_id: i64) -> bool {
        self.pending.lock().unwrap().contains(&query_id)
    }

    /// Claim a slot for this query. Returns `true` if newly claimed (the caller
    /// should enqueue it), `false` if a job is already queued or in flight.
    fn try_claim(&self, query_id: i64) -> bool {
        self.pending.lock().unwrap().insert(query_id)
    }

    /// Release the slot once the job finishes, so the next timer tick can
    /// re-enqueue the query if it's still stale.
    fn release(&self, query_id: i64) {
        self.pending.lock().unwrap().remove(&query_id);
    }
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
/// `incremental` *permits* narrowing the fetch to items updated since the last one,
/// but does not guarantee it — `resolve_since` still promotes the fetch to a full
/// one when the query was never fetched, constrains `updated:` itself, or is due a
/// pruning full fetch. `incremental: false` forces a full fetch unconditionally.
#[derive(Clone, Copy)]
pub struct SyncOpts {
    pub background: bool,
    pub incremental: bool,
    /// Age (seconds) past which an `incremental` sync is upgraded to a full fetch
    /// so pruning can run. Ignored when `incremental` is false (already full).
    pub full_fetch_interval_secs: i64,
    /// How far this sync's absences are trusted when pruning. See [`PruneTrust`].
    pub prune_trust: PruneTrust,
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

/// Whether a finished fetch's result set may be used to prune.
///
/// Only a full fetch is authoritative, so an incremental one never prunes. Two things
/// then disqualify an otherwise-full fetch:
///
/// - **Truncated** (`total_count >= SEARCH_RESULT_CAP`): GitHub may have cut the
///   result set off, so we can't tell an item that left the query from one that was
///   cut off.
/// - **Incomplete** (`!complete`): this walk lost data (an unparseable node, a
///   partial GraphQL error, a page walk that couldn't continue), so a missing key
///   doesn't prove the item left. Pruning would delete live rows and their read
///   markers.
fn may_prune(is_full: bool, total_count: usize, complete: bool) -> bool {
    is_full && total_count < SEARCH_RESULT_CAP && complete
}

/// Render item keys as `owner/repo#number` for a log line. The format is a contract with
/// whatever reads those lines back, so it lives at the log site rather than in `db`.
fn item_key_labels(keys: &[db::ItemKey]) -> Vec<String> {
    keys.iter()
        .map(|(owner, name, number)| format!("{owner}/{name}#{number}"))
        .collect()
}

/// How far a single absence from one fetch is trusted to mean "this item left the
/// query".
///
/// Stated per sync rather than inferred from `incremental`: that flag is about fetch
/// *scope* (and `resolve_since` promotes incremental walks to full ones freely), so
/// reading a deletion policy out of it would mean the next `incremental: false`
/// construction site added for some non-user reason silently acquires
/// delete-on-first-absence. This is the only safety property in the prune path, so it
/// gets said out loud.
#[derive(Clone, Copy)]
pub enum PruneTrust {
    /// Automatic sync: require `db::PRUNE_STRIKES` consecutive absences, so a paged
    /// walk that raced an update can't destroy a live row's read marker.
    Corroborate,
    /// The user explicitly asked for a full resync. Delete on the first absence rather
    /// than making them press `S` twice; if that races a mid-walk update it costs one
    /// row its read marker, which is their call to make and not the timer's.
    Immediate,
}

impl PruneTrust {
    fn strikes_required(self) -> i64 {
        match self {
            PruneTrust::Corroborate => db::PRUNE_STRIKES,
            PruneTrust::Immediate => 1,
        }
    }
}

/// Resolve the `updated:>=` threshold for one sync. `Some(ts)` narrows the fetch to
/// items updated since `ts`; `None` means a full fetch — the authoritative result
/// set, and the only kind that may prune.
///
/// A full fetch is chosen when the caller forced one (`incremental: false`), the
/// query was never fetched, the query already constrains `updated:` itself (so it
/// would not be narrowed anyway, and its fetch is authoritative), a forced full
/// fetch is due, or a DB read fails (degrading to a full fetch is safe, just more
/// work).
///
/// The periodic upgrade exists because an incremental fetch can never observe an
/// item *leaving* the result set: `updated:>=` is ANDed onto the query, so an item
/// that stopped matching is simply never returned again and its cached row lingers
/// with a stale `state`. Only the prune after a full fetch removes it.
async fn resolve_since(
    pool: &SqlitePool,
    query_id: i64,
    query_str: &str,
    opts: SyncOpts,
) -> Option<String> {
    // Phrased as the reasons to fetch in full, matching the list above; the
    // equivalent `want_incremental` form needs three nested negations to read.
    let must_fetch_full = !opts.incremental
        || github::constrains_updated(query_str)
        || db::is_full_fetch_due(pool, query_id, opts.full_fetch_interval_secs)
            .await
            .unwrap_or(true);
    if must_fetch_full {
        return None;
    }
    db::updated_since(pool, query_id, INCREMENTAL_OVERLAP_SECS)
        .await
        .ok()
        .flatten()
}

/// Fetch fresh results from GitHub API page by page, upserting each page immediately
/// so the UI can show results as they arrive rather than waiting for all pages.
#[instrument(
    skip(pool, gh, query_str, opts, tx, gate),
    fields(
        background = opts.background,
        incremental = opts.incremental,
        full_fetch = tracing::field::Empty
    )
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
    // Incremental fetches narrow the query to items updated since the last fetch;
    // `None` means a full fetch. See `resolve_since` for when each is chosen — in
    // particular, an incremental sync is periodically upgraded to a full one so the
    // prune below can drop items that silently stopped matching the query.
    let since = resolve_since(&pool, query_id, &query_str, opts).await;
    // Only a full fetch (no `since`) can tell us what fell out, so only then do we
    // collect keys to prune against. Being full is necessary but not sufficient — a
    // truncated or lossy walk is disqualified too; see `may_prune`.
    let is_full = since.is_none();
    // `incremental` on the span is the *permission* to narrow the fetch; this is what the walk
    // actually did, and it decides whether pruning is even possible. Recorded on the span so
    // every line of this sync — "sync done", the prune outcome, any warning — carries it.
    tracing::Span::current().record("full_fetch", is_full);
    let mut keep_keys: Vec<db::ItemKey> = Vec::new();
    // `last_full_fetch_at` as it stands *before* the walk. Handed to the prune so it
    // can detect a concurrent full fetch finishing first — see `prune_missing_items`.
    let last_full_fetch_before_walk = if is_full {
        db::last_full_fetch_at(&pool, query_id)
            .await
            .unwrap_or_else(|e| {
                // `None` will mismatch a non-NULL current stamp and so skip this
                // cycle's prune — the safe direction, but log it rather than leaving a
                // query that mysteriously never prunes.
                warn!(error = %e, "could not read last_full_fetch_at; prune may be skipped");
                None
            })
    } else {
        None
    };

    // Reload the query's items from the DB and push them to the UI. Called after
    // each page (incremental display) and after a prune actually removes rows.
    let reload = || load_items_task(pool.clone(), query_id, opts.background, tx.clone());

    let mut after: Option<String> = None;
    // Raw `nodes` feed the `SEARCH_RESULT_CAP` comparison, which must reflect what
    // GitHub returned; cached items are what the UI's "Synced N items" reports. They
    // differ whenever a node failed to parse.
    let mut total_node_count = 0usize;
    let mut total_item_count = 0usize;
    // Whether `keep_keys` may be trusted as the complete result set. Cleared by any
    // page that lost nodes (parse failure or partial GraphQL error) and by a walk
    // that ended without exhausting the pages — in either case a key could be
    // missing for a reason other than "left the query", and pruning would delete a
    // live row along with its read marker.
    let mut complete = true;

    loop {
        let result = github::search_page(
            &gh,
            query_id,
            &query_str,
            since.as_deref(),
            after.as_deref(),
        )
        .await;
        // Both failures end the walk, so take them as guards and keep the page
        // handling — the bulk of this loop — at one level of indentation.
        let page = match result {
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
            Err(github::SearchError::Other(api_err)) => {
                warn!(error = %api_err, "sync failed");
                // Record the failed attempt so the full walk isn't retried every sync.
                // Stamps only `last_full_fetch_attempt_at`, so this can't be mistaken
                // for a completed walk — see `mark_full_fetch_attempted`.
                if is_full && let Err(db_err) = db::mark_full_fetch_attempted(&pool, query_id).await
                {
                    warn!(error = %db_err, "failed to record full-fetch attempt");
                }
                let _ = tx
                    .send(AppMessage::SyncError {
                        query_id,
                        error: format!("GitHub API error: {api_err}"),
                        background: opts.background,
                    })
                    .await;
                return;
            }
            Ok(page) => page,
        };

        if is_full {
            keep_keys.extend(
                page.items
                    .iter()
                    .map(|item| (item.repo_owner.clone(), item.repo_name.clone(), item.number)),
            );
        }
        // Upsert this page's items into SQLite in one transaction.
        if let Err(e) = db::upsert_items(&pool, &page.items).await {
            let _ = tx
                .send(AppMessage::SyncError {
                    query_id,
                    error: format!("db write error: {e}"),
                    background: opts.background,
                })
                .await;
            return;
        }
        total_node_count += page.node_count;
        total_item_count += page.items.len();
        complete &= page.faithful;
        debug!(
            page_items = page.items.len(),
            page_nodes = page.node_count,
            total_node_count,
            complete,
            "fetched page"
        );

        // Reload from DB after each page so the UI shows results immediately.
        // A page that produced no items wrote nothing, so re-reading the whole
        // list would ship an identical snapshot — skip it. That matters in
        // steady state, where most incremental syncs find nothing at all and
        // would otherwise re-read (and re-diff) every cached row every minute.
        if !page.items.is_empty() {
            reload().await;
        }

        // Stop when GitHub reports no further pages, or defensively if
        // it claims another page but hands back no cursor to fetch it —
        // the latter leaves the result set unfinished, so it must not be
        // treated as authoritative for pruning.
        if !page.has_next_page {
            break;
        }
        let Some(cursor) = page.end_cursor else {
            warn!("search reported another page but no cursor; stopping walk");
            complete = false;
            break;
        };
        after = Some(cursor);
    }

    // After a full fetch that we can vouch for, drop cached items the query no
    // longer returns (e.g. a PR that was merged and left an `is:open` query).
    if may_prune(is_full, total_node_count, complete) {
        let strikes = opts.prune_trust.strikes_required();
        match db::prune_missing_items(
            &pool,
            query_id,
            &keep_keys,
            strikes,
            last_full_fetch_before_walk.as_deref(),
        )
        .await
        {
            // One line per prunable walk, even when nothing was absent: that count is the
            // denominator when reading how often a transient absence happens. A skip is
            // logged separately because it observed nothing, so it is not evidence.
            Ok(db::PruneOutcome::Skipped { reason }) => info!(reason, "prune skipped"),
            Ok(db::PruneOutcome::Considered {
                cached,
                absent,
                deleted,
                absent_keys,
                deleted_keys,
            }) => {
                // `strikes` distinguishes a corroborating walk from a `PruneTrust::Immediate`
                // one, which deletes on the first absence: read as if they were the same, an
                // immediate deletion looks like an item that left after two observations.
                info!(
                    strikes,
                    cached,
                    absent,
                    deleted,
                    absent_keys = ?item_key_labels(&absent_keys),
                    deleted_keys = ?item_key_labels(&deleted_keys),
                    "prune considered"
                );
                // Only reload when rows were actually removed — the final per-page
                // reload already reflects every upsert.
                if deleted > 0 {
                    reload().await;
                }
            }
            Err(e) => {
                // Logged as well as surfaced: a failed prune leaves no line of its own, so a
                // query whose prune keeps erroring would just be missing from the log rather
                // than visibly broken — and its walks would drop silently out of the
                // denominator the absence measurement is read against.
                warn!(error = %e, "prune failed");
                let _ = tx
                    .send(AppMessage::Status(format!("prune error: {e}")))
                    .await;
            }
        }
    } else if is_full && total_node_count < SEARCH_RESULT_CAP {
        // Reaching here with a full, untruncated fetch leaves only one disqualifier,
        // so `may_prune`'s rule doesn't have to be restated to name it. Truncation is
        // not worth a warning: it's a property of the query, not of this attempt.
        warn!("skipping prune: incomplete result set");
    }

    // Mark the query as freshly fetched only after all pages are done. Nothing stamps
    // `last_full_fetch_at` before the paging loop finishes: the API-error return above
    // records its failed attempt in `last_full_fetch_attempt_at` instead, which defers
    // the retry without claiming a completion the prune guard would trust.
    //
    // A completed full fetch stamps even when it couldn't prune (truncated, or the
    // walk lost data). Withholding the stamp to "retry sooner" is a trap: some
    // conditions are permanent, not transient — a query spanning a SAML/SSO- or
    // OAuth-App-restricted org gets `FORBIDDEN` errors alongside its `data` on every
    // single request, so `complete` would be false forever. That would promote every
    // background sync to a full re-page (10+ requests per minute for a large query,
    // into a secondary rate limit) while never once pruning. Stamping bounds the
    // retry to one attempt per `full_fetch_interval_secs`; the cost is that a ghost
    // may survive one extra interval.
    if let Err(e) = db::mark_fetched(&pool, query_id, is_full).await {
        let _ = tx
            .send(AppMessage::SyncError {
                query_id,
                error: format!("mark fetched error: {e}"),
                background: opts.background,
            })
            .await;
        return;
    }
    info!(total_node_count, total_item_count, complete, "sync done");
    let _ = tx
        .send(AppMessage::SyncDone {
            query_id,
            count: total_item_count,
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
/// Default interval at which an incremental background sync is upgraded to a full
/// fetch, so `db::prune_missing_items` can drop rows that silently left the result
/// set. For a query whose results fit one 100-item page this costs the exact same
/// single GraphQL request as an incremental sync, so it is nearly free for typical
/// queries; a large result set pays one extra request per 100 items per interval.
pub const DEFAULT_FULL_FETCH_INTERVAL_SECS: u64 = 1800;
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
///
/// Built via [`MaintenanceConfig::effective`], like [`SyncConfig`], so the clamping
/// happens once at the front-end boundary rather than silently on every sweep.
#[derive(Debug, Clone, Copy)]
pub struct MaintenanceConfig {
    /// Age (days) past which an item's `body` is cleared. Terminal-state items are
    /// cleared regardless of age. At least 1.
    retention_days: u64,
    /// Per-query cap on cached rows; read overflow beyond it is pruned. At least
    /// `SEARCH_RESULT_CAP`.
    max_items_per_query: u64,
}

impl MaintenanceConfig {
    /// Clamp both values to floors that keep the sweep from fighting the sync.
    ///
    /// `retention_days = 0` would clear essentially every body. `max_items_per_query`
    /// below `SEARCH_RESULT_CAP` cannot be honoured at all: the rows it deletes as
    /// overflow are still inside the query's live result set, so the next full fetch
    /// re-inserts them, unread (see `db::upsert_item`).
    ///
    /// Back when a full fetch happened at most once per session, that delete/re-insert
    /// flap happened at most once too. Now that full fetches are on a timer it would
    /// repeat every `full_fetch_interval_secs`, so the setting is ruled out rather
    /// than documented.
    pub fn effective(retention_days: u64, max_items_per_query: u64) -> Self {
        let effective = Self {
            retention_days: retention_days.max(1),
            max_items_per_query: max_items_per_query.max(SEARCH_RESULT_CAP as u64),
        };
        if max_items_per_query < effective.max_items_per_query {
            info!(
                configured = max_items_per_query,
                effective = effective.max_items_per_query,
                "max_items_per_query raised to the search result cap"
            );
        }
        if retention_days < effective.retention_days {
            info!(
                configured = retention_days,
                effective = effective.retention_days,
                "retention_days raised to its floor"
            );
        }
        effective
    }
}

/// Clamp a configured interval to at least `MIN_SYNC_INTERVAL_SECS`.
pub fn effective_interval(secs: u64) -> u64 {
    secs.max(MIN_SYNC_INTERVAL_SECS)
}

/// Tunables for background sync scheduling. Built via [`SyncConfig::effective`] so
/// the clamping happens once, at the front-end boundary.
///
/// Fields are private so `effective` is the only way to build one: `interval_secs`
/// reaches `tokio::time::interval`, which panics on a zero duration, and the clamping
/// is what rules that out.
#[derive(Debug, Clone, Copy)]
pub struct SyncConfig {
    /// Auto-refresh interval, which doubles as the cache-staleness threshold.
    interval_secs: u64,
    /// How often an incremental sync is upgraded to a full (pruning) fetch.
    full_fetch_interval_secs: u64,
}

impl SyncConfig {
    /// Clamp both values: the interval to `MIN_SYNC_INTERVAL_SECS`, and the
    /// full-fetch interval to at least the sync interval — a full fetch cannot come
    /// due more often than a sync happens, so a smaller value would only mislead.
    /// `full_fetch_interval_secs = 0` therefore means "every sync is a full fetch",
    /// which is the intended escape hatch.
    pub fn effective(interval_secs: u64, full_fetch_interval_secs: u64) -> Self {
        let interval_secs = effective_interval(interval_secs);
        Self {
            interval_secs,
            full_fetch_interval_secs: full_fetch_interval_secs.max(interval_secs),
        }
    }

    /// Auto-refresh interval, clamped to at least `MIN_SYNC_INTERVAL_SECS`.
    pub fn interval_secs(&self) -> u64 {
        self.interval_secs
    }

    /// Cache-staleness threshold: a query is stale once it is this old.
    pub fn stale_secs(&self) -> i64 {
        Self::to_secs(self.interval_secs)
    }

    /// Age past which an incremental sync is upgraded to a full fetch.
    pub fn full_fetch_interval_secs(&self) -> i64 {
        Self::to_secs(self.full_fetch_interval_secs)
    }

    /// Saturate rather than cast: a configured value above `i64::MAX` would wrap
    /// *negative*, and the DB reads a negative threshold as "always overdue" — so a
    /// nonsensically large setting would silently mean the exact opposite of what it
    /// says. Saturating keeps an absurd value merely absurd.
    fn to_secs(secs: u64) -> i64 {
        i64::try_from(secs).unwrap_or(i64::MAX)
    }
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
    sync: SyncConfig,
    gate: RateLimitGate,
    pending: SyncCoalescer,
) {
    while let Some(job) = rx.recv().await {
        // Skip (don't hit the API) while rate-limited; the query stays stale and
        // will be re-enqueued once the gate reopens.
        if !gate.is_open(Utc::now().timestamp()) {
            debug!(query_id = job.query_id, "skip sync: rate-limited");
        } else if db::is_cache_stale(&pool, job.query_id, sync.stale_secs())
            .await
            .unwrap_or(true)
        {
            sync_task(
                pool.clone(),
                gh.clone(),
                job.query_id,
                job.query_str,
                SyncOpts {
                    background: true,
                    incremental: true,
                    full_fetch_interval_secs: sync.full_fetch_interval_secs(),
                    prune_trust: PruneTrust::Corroborate,
                },
                tx.clone(),
                gate.clone(),
            )
            .await;
        }
        // Every exit path (rate-limited, synced, skipped-as-fresh, or failed):
        // release the coalescing slot so the next timer tick can re-enqueue this
        // query, then report the job as done.
        pending.release(job.query_id);
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
    pending: &SyncCoalescer,
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
        // Already queued or in flight: skip without touching the DB, so a long
        // offline stretch doesn't re-run is_cache_stale for every query each tick.
        if pending.is_pending(q.id) {
            continue;
        }
        if db::is_cache_stale(pool, q.id, stale_secs)
            .await
            .unwrap_or(true)
        {
            // try_claim re-checks membership under the lock (a concurrent enqueue
            // could have claimed this query during the await above), so enqueue
            // only when we newly claim the slot — a long offline stretch can't
            // pile up duplicate jobs.
            if pending.try_claim(q.id) {
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
    sync: SyncConfig,
    gate: RateLimitGate,
    pending: SyncCoalescer,
) {
    // `SyncConfig` rather than a bare `u64`, for the invariants its constructor
    // enforces — see [`SyncConfig`].
    let mut interval =
        tokio::time::interval(tokio::time::Duration::from_secs(sync.interval_secs()));
    interval.tick().await; // skip the immediate first tick
    loop {
        interval.tick().await;
        enqueue_stale_queries(
            &pool,
            &sync_tx,
            &app_tx,
            None,
            sync.stale_secs(),
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
    // Already clamped by `MaintenanceConfig::effective`.
    let retention_days = cfg.retention_days as i64;
    let max_items = cfg.max_items_per_query as i64;
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
    /// Unconditional GitHub sync for a root query. Normally incremental (only items
    /// updated since the last fetch), but promoted to a pruning full fetch when one
    /// is due — see `resolve_since`. Foreground, so a prune applies live rather than
    /// waiting behind the banner.
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
    /// GitHub sync only if the query's cache is stale (sends `SyncStarted` if it
    /// runs). Incremental like `Sync`, and promoted to a full fetch on the same
    /// terms.
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
        sync: SyncConfig,
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
        let interval = sync.interval_secs;
        info!(
            sync_interval_secs = interval,
            full_fetch_interval_secs = sync.full_fetch_interval_secs,
            "engine started"
        );
        // Shared gate that pauses background sync after a rate limit is hit.
        let gate = RateLimitGate::new();
        // Coalesces background sync jobs: at most one queued/in-flight job per
        // query, so a long offline stretch can't pile up duplicates.
        let pending = SyncCoalescer::new();

        // Spawn the sequential background sync worker.
        tokio::spawn(sync_worker_task(
            pool.clone(),
            gh.clone(),
            sync_job_rx,
            msg_tx.clone(),
            sync,
            gate.clone(),
            pending.clone(),
        ));
        // Spawn the periodic refresh timer.
        tokio::spawn(refresh_timer_task(
            pool.clone(),
            sync_job_tx.clone(),
            msg_tx.clone(),
            sync,
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
            sync,
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
#[allow(clippy::too_many_arguments)] // pool/gh/channels/sync/gate/pending are all genuinely needed
async fn command_loop(
    pool: SqlitePool,
    gh: Octocrab,
    mut cmd_rx: mpsc::Receiver<EngineCommand>,
    msg_tx: mpsc::Sender<AppMessage>,
    sync_tx: mpsc::Sender<SyncJob>,
    sync: SyncConfig,
    gate: RateLimitGate,
    pending: SyncCoalescer,
) {
    let stale_secs = sync.stale_secs();
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
                        full_fetch_interval_secs: sync.full_fetch_interval_secs(),
                        prune_trust: PruneTrust::Corroborate,
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
                        // Ignored: the fetch is already full.
                        full_fetch_interval_secs: sync.full_fetch_interval_secs(),
                        // The user asked for this explicitly, so one press is enough.
                        prune_trust: PruneTrust::Immediate,
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
                                full_fetch_interval_secs: sync.full_fetch_interval_secs(),
                                prune_trust: PruneTrust::Corroborate,
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
    use crate::test_support::test_pool;
    use rstest::rstest;

    #[test]
    fn effective_interval_clamps_to_floor() {
        assert_eq!(effective_interval(0), MIN_SYNC_INTERVAL_SECS);
        assert_eq!(effective_interval(5), MIN_SYNC_INTERVAL_SECS);
        assert_eq!(effective_interval(60), 60);
        assert_eq!(effective_interval(120), 120);
    }

    #[rstest]
    // An incremental fetch is never authoritative.
    #[case::incremental(false, 10, true, false)]
    #[case::full_and_complete(true, 10, true, true)]
    // Lost data mid-walk: a missing key doesn't prove the item left, so pruning would
    // delete live rows. (It still stamps both fetch columns — see `sync_task`.)
    #[case::full_but_incomplete(true, 10, false, false)]
    // Truncated by GitHub: can't tell an item that left from one that was cut off.
    #[case::truncated(true, SEARCH_RESULT_CAP, true, false)]
    #[case::truncated_and_incomplete(true, SEARCH_RESULT_CAP, false, false)]
    fn may_prune_cases(
        #[case] is_full: bool,
        #[case] total_count: usize,
        #[case] complete: bool,
        #[case] want: bool,
    ) {
        assert_eq!(may_prune(is_full, total_count, complete), want);
    }

    #[rstest]
    // A cap below the search cap can't be honoured — the rows it deletes are still in
    // the query's live result set and come back unread on the next full fetch.
    #[case::cap_raised_to_search_cap(90, 200, 90, SEARCH_RESULT_CAP as u64)]
    #[case::zero_cap_raised(90, 0, 90, SEARCH_RESULT_CAP as u64)]
    // `retention_days = 0` would clear essentially every cached body.
    #[case::zero_retention_floored(0, 1500, 1, 1500)]
    #[case::defaults_pass_through(
        DEFAULT_RETENTION_DAYS,
        DEFAULT_MAX_ITEMS_PER_QUERY,
        DEFAULT_RETENTION_DAYS,
        DEFAULT_MAX_ITEMS_PER_QUERY
    )]
    fn maintenance_config_clamps_to_floors(
        #[case] retention_days: u64,
        #[case] max_items: u64,
        #[case] want_retention: u64,
        #[case] want_max_items: u64,
    ) {
        let cfg = MaintenanceConfig::effective(retention_days, max_items);
        assert_eq!(cfg.retention_days, want_retention);
        assert_eq!(cfg.max_items_per_query, want_max_items);
    }

    #[rstest]
    // `0` is the escape hatch: every sync becomes a full fetch, i.e. due as often
    // as a sync can happen.
    #[case::zero_means_every_sync(60, 0, 60, 60)]
    // The interval is floored, and a larger full-fetch interval passes through.
    #[case::interval_floored(5, 1800, MIN_SYNC_INTERVAL_SECS as i64, 1800)]
    // A full fetch can't come due more often than a sync runs.
    #[case::clamped_up_to_interval(3600, 1800, 3600, 3600)]
    #[case::defaults(
        DEFAULT_SYNC_INTERVAL_SECS,
        DEFAULT_FULL_FETCH_INTERVAL_SECS,
        DEFAULT_SYNC_INTERVAL_SECS as i64,
        DEFAULT_FULL_FETCH_INTERVAL_SECS as i64
    )]
    // An absurd value saturates instead of wrapping negative, which the DB would
    // read as "always overdue" — the opposite of what the setting asks for.
    #[case::saturates_instead_of_wrapping(u64::MAX, u64::MAX, i64::MAX, i64::MAX)]
    fn sync_config_effective_cases(
        #[case] interval: u64,
        #[case] full_fetch: u64,
        #[case] want_stale: i64,
        #[case] want_full_fetch: i64,
    ) {
        let cfg = SyncConfig::effective(interval, full_fetch);
        assert_eq!(cfg.stale_secs(), want_stale);
        assert_eq!(cfg.full_fetch_interval_secs(), want_full_fetch);
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

    /// `SyncOpts` for a background-style incremental sync with the given full-fetch
    /// deadline. A negative deadline forces "overdue" without touching the clock.
    fn incremental_opts(full_fetch_interval_secs: i64) -> SyncOpts {
        SyncOpts {
            background: true,
            incremental: true,
            full_fetch_interval_secs,
            prune_trust: PruneTrust::Corroborate,
        }
    }

    /// A never-fetched query has nothing to be incremental against, so the first
    /// sync is always a full fetch.
    #[tokio::test]
    async fn resolve_since_none_when_never_fetched() {
        let (pool, _file) = test_pool().await;
        let qid = db::upsert_query(&pool, "repo:o/a is:pr", "pull_request", None)
            .await
            .expect("q");

        assert_eq!(
            resolve_since(&pool, qid, "repo:o/a is:pr", incremental_opts(1800)).await,
            None
        );
    }

    /// `incremental: false` (the `S` key / `FullResync`) always fetches in full,
    /// regardless of how recently a full fetch happened.
    #[tokio::test]
    async fn resolve_since_forced_full_ignores_timestamps() {
        let (pool, _file) = test_pool().await;
        let qid = db::upsert_query(&pool, "repo:o/a is:pr", "pull_request", None)
            .await
            .expect("q");
        db::mark_fetched(&pool, qid, true).await.expect("mark");

        let opts = SyncOpts {
            background: false,
            incremental: false,
            full_fetch_interval_secs: 1800,
            prune_trust: PruneTrust::Immediate,
        };
        assert_eq!(
            resolve_since(&pool, qid, "repo:o/a is:pr", opts).await,
            None
        );
    }

    /// The normal case: a recent full fetch means the next sync may narrow itself.
    #[tokio::test]
    async fn resolve_since_some_after_recent_full_fetch() {
        let (pool, _file) = test_pool().await;
        let qid = db::upsert_query(&pool, "repo:o/a is:pr", "pull_request", None)
            .await
            .expect("q");
        db::mark_fetched(&pool, qid, true).await.expect("mark");

        // The threshold's RFC3339 shape is `db::updated_since`'s concern, tested there.
        assert!(
            resolve_since(&pool, qid, "repo:o/a is:pr", incremental_opts(1800))
                .await
                .is_some(),
            "a recent full fetch must let the next sync narrow itself"
        );
    }

    /// Regression test for the ghost bug: once the full-fetch deadline passes, an
    /// otherwise-incremental sync is upgraded to a full fetch so `prune_missing_items`
    /// can drop items that silently stopped matching the query.
    #[tokio::test]
    async fn resolve_since_none_when_full_fetch_overdue() {
        let (pool, _file) = test_pool().await;
        let qid = db::upsert_query(&pool, "repo:o/a is:pr", "pull_request", None)
            .await
            .expect("q");
        db::mark_fetched(&pool, qid, true).await.expect("mark");

        assert_eq!(
            resolve_since(&pool, qid, "repo:o/a is:pr", incremental_opts(-1)).await,
            None
        );
    }

    /// A query that constrains `updated:` itself is never narrowed further by
    /// `apply_updated_since`, so its fetch is authoritative and must count as full
    /// — otherwise `sync_task` would skip the prune for a result set it did fetch
    /// in full.
    #[tokio::test]
    async fn resolve_since_none_when_query_constrains_updated() {
        let (pool, _file) = test_pool().await;
        let query = "is:pr updated:>2026-01-01";
        let qid = db::upsert_query(&pool, query, "pull_request", None)
            .await
            .expect("q");
        db::mark_fetched(&pool, qid, true).await.expect("mark");

        assert_eq!(
            resolve_since(&pool, qid, query, incremental_opts(1800)).await,
            None
        );
    }

    /// While the worker isn't draining (e.g. offline, jobs stuck in flight),
    /// repeated timer ticks must not pile up duplicate jobs for the same query:
    /// each stale query is enqueued at most once until its slot is released.
    #[tokio::test]
    async fn enqueue_stale_coalesces_duplicate_jobs() {
        let (pool, _file) = test_pool().await;

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
        let pending = SyncCoalescer::new();

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
        let (pool, _file) = test_pool().await;
        let q1 = db::upsert_query(&pool, "repo:o/a is:pr", "pull_request", None)
            .await
            .expect("q1");

        let (sync_tx, mut sync_rx) = mpsc::channel::<SyncJob>(256);
        let (app_tx, _app_rx) = mpsc::channel::<AppMessage>(256);
        let gate = RateLimitGate::new();
        let pending = SyncCoalescer::new();

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
        pending.release(q1);
        enqueue_stale_queries(&pool, &sync_tx, &app_tx, None, 60, &gate, &pending).await;
        let second = sync_rx.recv().await.expect("re-enqueued job");
        assert_eq!(second.query_id, q1, "released slot allows re-enqueue");
    }
}
