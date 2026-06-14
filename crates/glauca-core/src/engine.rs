// glauca-core::engine — framework 非依存の非同期エンジン処理。
// TUI/GUI 双方から利用する。$EDITOR 起動・端末制御などフロントエンド固有の処理は呼び出し側に残し、
// ここには pool/gh/mpsc チャネルだけで完結するタスクと、それらが受け渡すメッセージ型を集約する。

use crate::logic::cached_item_to_item_entry;
use crate::types::{CommentEntry, FilterStreamEntry, ItemEntry, MergeStrategy, QueryEntry};
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
