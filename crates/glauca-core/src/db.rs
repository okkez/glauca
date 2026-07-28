use anyhow::Result;
use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};
use std::path::PathBuf;
use std::time::Duration;

pub async fn open_pool(db_path: &PathBuf) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        // Block briefly on a locked DB instead of failing immediately — chiefly so
        // a concurrent write during the maintenance pass's VACUUM (which needs
        // exclusive access) waits its turn instead of erroring out its sync cycle.
        .busy_timeout(Duration::from_secs(30));
    let pool = SqlitePool::connect_with(options).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

pub fn default_db_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("glauca")
        .join("cache.db")
}

pub struct QueryRecord {
    pub id: i64,
    pub query: String,
    pub kind: String,
    /// Optional display name. If None, the query string is used as the label.
    pub name: Option<String>,
}

/// List all saved queries ordered by position.
pub async fn list_queries(pool: &SqlitePool) -> Result<Vec<QueryRecord>> {
    let rows = sqlx::query!(
        "SELECT id, query, kind, name FROM queries ORDER BY position ASC, created_at ASC"
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| QueryRecord {
            id: r.id,
            query: r.query,
            kind: r.kind,
            name: r.name,
        })
        .collect())
}

// ── Filter stream types & functions ─────────────────────────────────────────

pub struct FilterStreamRecord {
    pub id: i64,
    pub parent_id: i64,
    pub name: String,
    pub filter: String,
}

/// List filter streams for a given parent query, ordered by position.
pub async fn list_filter_streams(
    pool: &SqlitePool,
    parent_id: i64,
) -> Result<Vec<FilterStreamRecord>> {
    let rows = sqlx::query!(
        "SELECT id, parent_id, name, filter FROM filter_streams WHERE parent_id = ? ORDER BY position ASC, created_at ASC",
        parent_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| FilterStreamRecord {
            id: r.id.expect("id is NOT NULL"),
            parent_id: r.parent_id,
            name: r.name,
            filter: r.filter,
        })
        .collect())
}

/// Insert a new filter stream under a parent query and return its id.
pub async fn upsert_filter_stream(
    pool: &SqlitePool,
    parent_id: i64,
    name: &str,
    filter: &str,
) -> Result<i64> {
    let row = sqlx::query!(
        r#"
        INSERT INTO filter_streams (parent_id, name, filter)
        VALUES (?, ?, ?)
        RETURNING id
        "#,
        parent_id,
        name,
        filter,
    )
    .fetch_one(pool)
    .await?;
    Ok(row.id.expect("id is NOT NULL"))
}

/// Delete a filter stream by id.
pub async fn delete_filter_stream(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query!("DELETE FROM filter_streams WHERE id = ?", id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Swap the `position` values of two filter streams (must share the same parent).
pub async fn swap_filter_stream_positions(
    pool: &SqlitePool,
    upper_id: i64,
    lower_id: i64,
) -> Result<()> {
    let upper_pos =
        sqlx::query_scalar!("SELECT position FROM filter_streams WHERE id = ?", upper_id)
            .fetch_one(pool)
            .await?;
    let lower_pos =
        sqlx::query_scalar!("SELECT position FROM filter_streams WHERE id = ?", lower_id)
            .fetch_one(pool)
            .await?;
    sqlx::query!(
        "UPDATE filter_streams SET position = ? WHERE id = ?",
        lower_pos,
        upper_id
    )
    .execute(pool)
    .await?;
    sqlx::query!(
        "UPDATE filter_streams SET position = ? WHERE id = ?",
        upper_pos,
        lower_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Update an existing query's display name and/or search string.
/// Passing `None` for `name` clears the display name (falls back to query string).
/// Resets both fetch timestamps: the cache is stale, and the edited query needs a
/// fresh full fetch to prune items the *old* query matched but the new one doesn't.
pub async fn update_query(
    pool: &SqlitePool,
    id: i64,
    name: Option<&str>,
    query: &str,
) -> Result<()> {
    sqlx::query!(
        "UPDATE queries SET name = ?, query = ?, last_fetched_at = NULL, last_full_fetch_at = NULL WHERE id = ?",
        name,
        query,
        id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Update an existing filter stream's name and filter string.
pub async fn update_filter_stream(
    pool: &SqlitePool,
    id: i64,
    name: &str,
    filter: &str,
) -> Result<()> {
    sqlx::query!(
        "UPDATE filter_streams SET name = ?, filter = ? WHERE id = ?",
        name,
        filter,
        id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete a query and its cached items (CASCADE).
pub async fn delete_query(pool: &SqlitePool, query_id: i64) -> Result<()> {
    sqlx::query!("DELETE FROM queries WHERE id = ?", query_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Swap the `position` values of two queries so that one moves past the other.
pub async fn swap_query_positions(pool: &SqlitePool, upper_id: i64, lower_id: i64) -> Result<()> {
    let upper_pos = sqlx::query_scalar!("SELECT position FROM queries WHERE id = ?", upper_id)
        .fetch_one(pool)
        .await?;
    let lower_pos = sqlx::query_scalar!("SELECT position FROM queries WHERE id = ?", lower_id)
        .fetch_one(pool)
        .await?;
    sqlx::query!(
        "UPDATE queries SET position = ? WHERE id = ?",
        lower_pos,
        upper_id
    )
    .execute(pool)
    .await?;
    sqlx::query!(
        "UPDATE queries SET position = ? WHERE id = ?",
        upper_pos,
        lower_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Upsert a query record and return its id.
///
/// `name` is the optional display name shown in the left pane.
/// If `None` (or empty string), the query string itself is used as the label.
pub async fn upsert_query(
    pool: &SqlitePool,
    query: &str,
    kind: &str,
    name: Option<&str>,
) -> Result<i64> {
    // Treat empty string the same as None.
    let name = name.filter(|s| !s.trim().is_empty());
    let row = sqlx::query!(
        r#"
        INSERT INTO queries (query, kind, name)
        VALUES (?, ?, ?)
        ON CONFLICT (query) DO UPDATE SET query = excluded.query, name = excluded.name
        RETURNING id
        "#,
        query,
        kind,
        name,
    )
    .fetch_one(pool)
    .await?;
    Ok(row.id)
}

/// Mark a query as freshly fetched. `full_fetch` additionally stamps
/// `last_full_fetch_at`, which gates the next forced full fetch — pass `true` only
/// when the fetch really covered the whole result set *and* the prune ran (or was
/// impossible), since that timestamp is a promise that ghosts were cleared. Both
/// columns are written in one statement so `last_full_fetch_at <= last_fetched_at`
/// always holds.
pub async fn mark_fetched(pool: &SqlitePool, query_id: i64, full_fetch: bool) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE queries
        SET last_fetched_at    = datetime('now'),
            last_full_fetch_at = CASE WHEN ? THEN datetime('now') ELSE last_full_fetch_at END
        WHERE id = ?
        "#,
        full_fetch,
        query_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// RFC3339 UTC threshold for an incremental fetch: the query's `last_fetched_at`
/// shifted back by `overlap_secs` (to tolerate clock skew and updates made while
/// the previous fetch was in flight). `None` when the query was never fetched —
/// the caller should then do a full fetch.
pub async fn updated_since(
    pool: &SqlitePool,
    query_id: i64,
    overlap_secs: i64,
) -> Result<Option<String>> {
    let modifier = format!("-{overlap_secs} seconds");
    let row = sqlx::query!(
        r#"
        SELECT strftime('%Y-%m-%dT%H:%M:%SZ', last_fetched_at, ?) AS "since?: String"
        FROM queries
        WHERE id = ?
        "#,
        modifier,
        query_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(row.since)
}

/// Delete cached items for `query_id` whose (repo_owner, repo_name, number) key
/// is absent from `keep` (the authoritative full-fetch result set). Returns the
/// number of rows deleted. Used only after an untruncated full fetch, so items
/// that no longer match the query are dropped instead of lingering as ghosts.
///
/// Read state is lost on departure: `upsert_item` never writes
/// `last_read_updated_at`, so an item that leaves the query and later matches
/// again comes back as unread. Unlike `prune_query_overflow` — which reasons at
/// length about avoiding exactly that, and protects the newest rows — this
/// function has no such protection, because deleting rows that no longer match is
/// its whole purpose. The behaviour is intended: a re-requested review or a
/// reopened issue is new actionable work, so surfacing it as unread is correct.
pub async fn prune_query_items(
    pool: &SqlitePool,
    query_id: i64,
    keep: &[(String, String, i64)],
) -> Result<u64> {
    use std::collections::HashSet;
    let keep_set: HashSet<(&str, &str, i64)> = keep
        .iter()
        .map(|(owner, name, number)| (owner.as_str(), name.as_str(), *number))
        .collect();

    let existing = sqlx::query!(
        r#"SELECT id AS "id!: i64", repo_owner, repo_name, number FROM items WHERE query_id = ?"#,
        query_id,
    )
    .fetch_all(pool)
    .await?;

    let stale_ids: Vec<i64> = existing
        .into_iter()
        .filter(|r| !keep_set.contains(&(r.repo_owner.as_str(), r.repo_name.as_str(), r.number)))
        .map(|r| r.id)
        .collect();

    if stale_ids.is_empty() {
        return Ok(0);
    }

    // Chunk to stay under SQLite's bound-variable limit on large prunes.
    let mut deleted = 0u64;
    for chunk in stale_ids.chunks(900) {
        let mut qb: sqlx::QueryBuilder<sqlx::Sqlite> =
            sqlx::QueryBuilder::new("DELETE FROM items WHERE id IN (");
        let mut sep = qb.separated(", ");
        for id in chunk {
            sep.push_bind(*id);
        }
        qb.push(")");
        deleted += qb.build().execute(pool).await?.rows_affected();
    }
    Ok(deleted)
}

/// Free cache space by clearing the (re-fetchable) `body` of items unlikely to be
/// read soon: those in a terminal state (`closed`/`merged`) or whose last activity
/// (`updated_at`) is older than `retention_days`. The row itself — title, state,
/// and the `last_read_updated_at` unread marker — is kept, so this never affects
/// unread state (`logic::is_item_unread`); the body is re-fetched on demand when
/// the item is opened. `body` is by far the largest column, so this reclaims most
/// of the cache size without the churn hazards of deleting rows. Returns the
/// number of rows whose body was cleared.
pub async fn clear_stale_bodies(pool: &SqlitePool, retention_days: i64) -> Result<u64> {
    let modifier = format!("-{retention_days} days");
    let res = sqlx::query!(
        r#"
        UPDATE items SET body = NULL
        WHERE body IS NOT NULL
          AND ( state IN ('closed', 'merged')
                OR updated_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?) )
        "#,
        modifier,
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Bound per-query row growth: keep the newest `max_rows` items (by `updated_at`)
/// for `query_id` and delete the older overflow — but only rows that are already
/// read, so an unread item is never dropped. The "read" predicate mirrors the
/// negation of `logic::is_item_unread`. Returns the number of rows deleted.
///
/// Deleting a read row that *still matched* the query would resurface it as unread
/// on the next sync (re-inserted with `last_read_updated_at = NULL`). That does not
/// happen here because overflow only exists once a query has accumulated more than
/// `max_rows` rows, the newest `max_rows` by `updated_at` are protected, and GitHub
/// search caps a query's live result set (~1000, `SEARCH_RESULT_CAP`) — so with a
/// `max_rows` comfortably above that cap the pruned old-`updated_at` rows are
/// effectively never re-returned by a sync. The `id` tiebreaker keeps the boundary
/// deterministic when `updated_at` values collide.
pub async fn prune_query_overflow(pool: &SqlitePool, query_id: i64, max_rows: i64) -> Result<u64> {
    let res = sqlx::query!(
        r#"
        DELETE FROM items
        WHERE query_id = ?
          AND last_read_updated_at IS NOT NULL
          AND updated_at <= last_read_updated_at
          AND id NOT IN (
              SELECT id FROM items
              WHERE query_id = ?
              ORDER BY updated_at DESC, id DESC
              LIMIT ?
          )
        "#,
        query_id,
        query_id,
        max_rows,
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Minimum freelist pages before a `VACUUM` is worth its full-file rewrite
/// (~1 MiB at the 4 KiB default page size). Below this the maintenance pass skips
/// it, so a 6-hourly sweep with nothing to reclaim doesn't needlessly rewrite the
/// whole DB (and briefly lock it).
const VACUUM_MIN_FREELIST_PAGES: i64 = 256;

/// Reclaim disk space freed by `clear_stale_bodies`/prunes, but only when enough
/// pages are actually free (see `VACUUM_MIN_FREELIST_PAGES`). SQLite keeps freed
/// pages in the file (default `auto_vacuum=0`) until `VACUUM` rewrites it. Returns
/// `true` if a VACUUM ran. Takes an exclusive lock while running.
pub async fn vacuum(pool: &SqlitePool) -> Result<bool> {
    let freelist: i64 = sqlx::query_scalar("PRAGMA freelist_count")
        .fetch_one(pool)
        .await?;
    if freelist < VACUUM_MIN_FREELIST_PAGES {
        return Ok(false);
    }
    sqlx::query("VACUUM").execute(pool).await?;
    Ok(true)
}

pub struct CachedItem {
    pub query_id: i64,
    pub kind: String,
    pub repo_owner: String,
    pub repo_name: String,
    /// Whether the repository is private (drives the lock indicator in the item
    /// list). Reflects the current visibility on each re-sync.
    pub repo_private: bool,
    pub number: i64,
    pub title: String,
    pub url: String,
    pub author: Option<String>,
    /// Author's GitHub avatar URL (drives the item-list avatar icon). Refreshed
    /// on each re-sync; `None` for older rows or authors without an avatar.
    pub author_avatar_url: Option<String>,
    pub state: String,
    pub updated_at: String,
    pub labels: String,
    pub comment_count: i64,
    pub requested_reviewers: String,
    pub reviews: String,
    pub body: Option<String>,
    pub assignees: String,
    pub is_draft: bool,
    pub created_at_item: Option<String>,
    pub base_ref: Option<String>,
    pub head_ref: Option<String>,
    pub review_decision: Option<String>,
    pub milestone: Option<String>,
    /// The `updated_at` the user had seen when they last read this item. `None`
    /// means never read. Preserved across re-syncs so a later update (advancing
    /// `updated_at`) makes the item unread again. See `logic::is_item_unread`.
    pub last_read_updated_at: Option<String>,
}

/// Insert or replace a cached item for a query.
pub async fn upsert_item(pool: &SqlitePool, item: &CachedItem) -> Result<()> {
    let is_draft_int = item.is_draft as i64;
    let repo_private_int = item.repo_private as i64;
    sqlx::query!(
        r#"
        INSERT INTO items
            (query_id, kind, repo_owner, repo_name, repo_private, number, title, url, author,
             author_avatar_url, state, updated_at, labels, comment_count, requested_reviewers,
             reviews, body, assignees, is_draft, created_at_item, base_ref, head_ref,
             review_decision, milestone, last_read_updated_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT (query_id, repo_owner, repo_name, number)
        DO UPDATE SET
            title               = excluded.title,
            url                 = excluded.url,
            author              = excluded.author,
            author_avatar_url   = excluded.author_avatar_url,
            state               = excluded.state,
            updated_at          = excluded.updated_at,
            labels              = excluded.labels,
            comment_count       = excluded.comment_count,
            requested_reviewers = excluded.requested_reviewers,
            reviews             = excluded.reviews,
            body                = excluded.body,
            assignees           = excluded.assignees,
            is_draft            = excluded.is_draft,
            repo_private        = excluded.repo_private,
            created_at_item     = excluded.created_at_item,
            base_ref            = excluded.base_ref,
            head_ref            = excluded.head_ref,
            review_decision     = excluded.review_decision,
            milestone           = excluded.milestone
            -- `last_read_updated_at` is intentionally NOT updated: it records the
            -- `updated_at` the user had read up to. `updated_at` above IS refreshed,
            -- so once a re-sync advances it past `last_read_updated_at` the item
            -- becomes unread again (is_item_unread). New rows insert it as NULL.
        "#,
        item.query_id,
        item.kind,
        item.repo_owner,
        item.repo_name,
        repo_private_int,
        item.number,
        item.title,
        item.url,
        item.author,
        item.author_avatar_url,
        item.state,
        item.updated_at,
        item.labels,
        item.comment_count,
        item.requested_reviewers,
        item.reviews,
        item.body,
        item.assignees,
        is_draft_int,
        item.created_at_item,
        item.base_ref,
        item.head_ref,
        item.review_decision,
        item.milestone,
        item.last_read_updated_at,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch all cached items for a query.
pub async fn fetch_items(pool: &SqlitePool, query_id: i64) -> Result<Vec<CachedItem>> {
    let rows = sqlx::query!(
        r#"
        SELECT query_id, kind, repo_owner, repo_name, repo_private, number, title, url, author,
               author_avatar_url, state, updated_at, labels, comment_count, requested_reviewers,
               reviews, body, assignees, is_draft, created_at_item, base_ref, head_ref,
               review_decision, milestone, last_read_updated_at
        FROM items
        WHERE query_id = ?
        ORDER BY updated_at DESC
        "#,
        query_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| CachedItem {
            query_id: r.query_id,
            kind: r.kind,
            repo_owner: r.repo_owner,
            repo_name: r.repo_name,
            repo_private: r.repo_private != 0,
            number: r.number,
            title: r.title,
            url: r.url,
            author: r.author,
            author_avatar_url: r.author_avatar_url,
            state: r.state,
            updated_at: r.updated_at,
            labels: r.labels,
            comment_count: r.comment_count,
            requested_reviewers: r.requested_reviewers,
            reviews: r.reviews,
            body: r.body,
            assignees: r.assignees,
            is_draft: r.is_draft != 0,
            created_at_item: r.created_at_item,
            base_ref: r.base_ref,
            head_ref: r.head_ref,
            review_decision: r.review_decision,
            milestone: r.milestone,
            last_read_updated_at: r.last_read_updated_at,
        })
        .collect())
}

/// Mark a single cached item as read (viewed). Identified by the same unique key
/// as `upsert_item`'s conflict target.
pub async fn mark_item_read(
    pool: &SqlitePool,
    query_id: i64,
    repo_owner: &str,
    repo_name: &str,
    number: i64,
) -> Result<()> {
    sqlx::query!(
        "UPDATE items SET last_read_updated_at = updated_at \
         WHERE query_id = ? AND repo_owner = ? AND repo_name = ? AND number = ?",
        query_id,
        repo_owner,
        repo_name,
        number,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark every cached item of a query as read. Used by "Mark all as read" on a
/// root query (filter-stream scope marks matching items individually instead).
pub async fn mark_all_items_read(pool: &SqlitePool, query_id: i64) -> Result<()> {
    sqlx::query!(
        "UPDATE items SET last_read_updated_at = updated_at WHERE query_id = ?",
        query_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether `ts` (a SQLite datetime string) is more than `max_age_secs` old.
/// SQLite does the arithmetic, so there is no Rust-side timestamp parse. Shared by
/// `is_cache_stale` and `is_full_fetch_due` so both use one definition of "too old".
async fn older_than(pool: &SqlitePool, ts: &str, max_age_secs: i64) -> Result<bool> {
    let old: bool = sqlx::query_scalar!(
        r#"SELECT (strftime('%s', 'now') - strftime('%s', ?)) > ? AS "old: bool""#,
        ts,
        max_age_secs,
    )
    .fetch_one(pool)
    .await?;
    Ok(old)
}

/// Check whether the cache for a query is stale (older than `max_age_secs`).
pub async fn is_cache_stale(pool: &SqlitePool, query_id: i64, max_age_secs: i64) -> Result<bool> {
    let row = sqlx::query!(
        r#"
        SELECT last_fetched_at
        FROM queries
        WHERE id = ?
        "#,
        query_id,
    )
    .fetch_one(pool)
    .await?;

    match row.last_fetched_at {
        None => Ok(true),
        Some(ts) => older_than(pool, &ts, max_age_secs).await,
    }
}

/// Whether `query_id` is due for a *full* (non-incremental) fetch: never full
/// fetched (NULL) or the last one is older than `max_age_secs`.
///
/// Only a full fetch is an authoritative result set, so only a full fetch may
/// prune rows that left the query (`prune_query_items`). This is therefore what
/// bounds how long a stale row can linger; see `engine::resolve_since`.
pub async fn is_full_fetch_due(
    pool: &SqlitePool,
    query_id: i64,
    max_age_secs: i64,
) -> Result<bool> {
    let row = sqlx::query!(
        r#"
        SELECT last_full_fetch_at
        FROM queries
        WHERE id = ?
        "#,
        query_id,
    )
    .fetch_one(pool)
    .await?;

    match row.last_full_fetch_at {
        None => Ok(true),
        Some(ts) => older_than(pool, &ts, max_age_secs).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    async fn test_pool() -> (SqlitePool, NamedTempFile) {
        let file = NamedTempFile::new().expect("tempfile");
        let pool = open_pool(&file.path().to_path_buf())
            .await
            .unwrap_or_else(|e| panic!("open pool: {e:#}"));
        (pool, file)
    }

    fn make_item(query_id: i64, number: i64, title: &str) -> CachedItem {
        CachedItem {
            query_id,
            kind: "pull_request".into(),
            repo_owner: "owner".into(),
            repo_name: "repo".into(),
            repo_private: false,
            number,
            title: title.to_string(),
            url: format!("https://github.com/owner/repo/pull/{number}"),
            author: Some("alice".into()),
            author_avatar_url: None,
            state: "open".into(),
            updated_at: "2026-05-22T00:00:00Z".into(),
            labels: "[]".into(),
            comment_count: 0,
            requested_reviewers: "[]".into(),
            reviews: "[]".into(),
            body: None,
            assignees: "[]".into(),
            is_draft: false,
            created_at_item: None,
            base_ref: None,
            head_ref: None,
            review_decision: None,
            milestone: None,
            last_read_updated_at: None,
        }
    }

    #[tokio::test]
    async fn delete_query_cascades_items() {
        let (pool, _file) = test_pool().await;

        let qid = upsert_query(&pool, "repo:owner/r is:pr", "pull_request", None)
            .await
            .expect("upsert query");

        upsert_item(&pool, &make_item(qid, 1, "First"))
            .await
            .expect("upsert");
        upsert_item(&pool, &make_item(qid, 2, "Second"))
            .await
            .expect("upsert");

        let items = fetch_items(&pool, qid).await.expect("fetch");
        assert_eq!(items.len(), 2);

        delete_query(&pool, qid).await.expect("delete query");

        // Items should be gone via ON DELETE CASCADE.
        let items = fetch_items(&pool, qid).await.expect("fetch after delete");
        assert_eq!(items.len(), 0);
    }

    #[tokio::test]
    async fn list_queries_returns_in_creation_order() {
        let (pool, _file) = test_pool().await;

        upsert_query(&pool, "query:first", "issue", None)
            .await
            .expect("upsert");
        upsert_query(&pool, "query:second", "pull_request", None)
            .await
            .expect("upsert");
        upsert_query(&pool, "query:third", "issue", None)
            .await
            .expect("upsert");

        let queries = list_queries(&pool).await.expect("list");
        assert_eq!(queries.len(), 3);
        assert_eq!(queries[0].query, "query:first");
        assert_eq!(queries[1].query, "query:second");
        assert_eq!(queries[2].query, "query:third");
    }

    #[tokio::test]
    async fn mark_read_sets_last_read_and_resync_resurfaces() {
        let (pool, _file) = test_pool().await;
        let qid = upsert_query(&pool, "repo:owner/r is:pr", "pull_request", None)
            .await
            .expect("upsert query");

        // Initially never read.
        let mut item = make_item(qid, 1, "PR");
        item.updated_at = "2026-05-22T00:00:00Z".into();
        upsert_item(&pool, &item).await.expect("upsert");
        assert_eq!(
            fetch_items(&pool, qid).await.expect("fetch")[0].last_read_updated_at,
            None
        );

        // Reading records the current updated_at.
        mark_item_read(&pool, qid, "owner", "repo", 1)
            .await
            .expect("mark read");
        assert_eq!(
            fetch_items(&pool, qid).await.expect("fetch")[0]
                .last_read_updated_at
                .as_deref(),
            Some("2026-05-22T00:00:00Z")
        );

        // A re-sync advancing updated_at preserves last_read_updated_at (DO UPDATE
        // omits it), so the item is now unread again (updated_at > last_read).
        item.updated_at = "2026-06-01T00:00:00Z".into();
        upsert_item(&pool, &item).await.expect("re-upsert");
        let fetched = fetch_items(&pool, qid).await.expect("fetch");
        assert_eq!(fetched[0].updated_at, "2026-06-01T00:00:00Z");
        assert_eq!(
            fetched[0].last_read_updated_at.as_deref(),
            Some("2026-05-22T00:00:00Z")
        );
    }

    #[tokio::test]
    async fn upsert_item_updates_on_conflict() {
        let (pool, _file) = test_pool().await;

        let qid = upsert_query(&pool, "repo:owner/r is:pr", "pull_request", None)
            .await
            .expect("upsert query");

        upsert_item(&pool, &make_item(qid, 1, "Original title"))
            .await
            .expect("first upsert");

        let mut updated = make_item(qid, 1, "Updated title");
        updated.state = "closed".into();
        upsert_item(&pool, &updated).await.expect("second upsert");

        let items = fetch_items(&pool, qid).await.expect("fetch");
        assert_eq!(items.len(), 1, "should not duplicate on conflict");
        assert_eq!(items[0].title, "Updated title");
        assert_eq!(items[0].state, "closed");
    }

    #[tokio::test]
    async fn upsert_and_fetch_items() {
        let (pool, _file) = test_pool().await;

        let qid = upsert_query(&pool, "repo:owner/r is:pr", "pull_request", None)
            .await
            .expect("upsert query");

        let item = make_item(qid, 1, "Fix bug");
        upsert_item(&pool, &item).await.expect("upsert item");

        let items = fetch_items(&pool, qid).await.expect("fetch items");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Fix bug");
        assert_eq!(items[0].author.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn upsert_and_fetch_preserves_private_and_avatar() {
        let (pool, _file) = test_pool().await;
        let qid = upsert_query(&pool, "repo:owner/r is:pr", "pull_request", None)
            .await
            .expect("upsert query");

        let mut item = make_item(qid, 1, "Fix bug");
        item.repo_private = true;
        item.author_avatar_url = Some("https://a/alice.png".into());
        upsert_item(&pool, &item).await.expect("upsert item");

        let items = fetch_items(&pool, qid).await.expect("fetch items");
        assert_eq!(items.len(), 1);
        assert!(items[0].repo_private);
        assert_eq!(
            items[0].author_avatar_url.as_deref(),
            Some("https://a/alice.png")
        );

        // Re-syncing the same item refreshes both fields (DO UPDATE SET).
        item.repo_private = false;
        item.author_avatar_url = Some("https://a/alice2.png".into());
        upsert_item(&pool, &item).await.expect("re-upsert item");

        let items = fetch_items(&pool, qid).await.expect("fetch items");
        assert_eq!(items.len(), 1);
        assert!(!items[0].repo_private);
        assert_eq!(
            items[0].author_avatar_url.as_deref(),
            Some("https://a/alice2.png")
        );
    }

    #[tokio::test]
    async fn filter_stream_crud() {
        let (pool, _file) = test_pool().await;

        let qid = upsert_query(&pool, "repo:owner/r is:pr", "pull_request", None)
            .await
            .expect("upsert query");

        let fid = upsert_filter_stream(&pool, qid, "Open PRs", "state:open")
            .await
            .expect("upsert filter stream");

        let streams = list_filter_streams(&pool, qid).await.expect("list");
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].id, fid);
        assert_eq!(streams[0].name, "Open PRs");
        assert_eq!(streams[0].filter, "state:open");

        delete_filter_stream(&pool, fid).await.expect("delete");
        let streams = list_filter_streams(&pool, qid)
            .await
            .expect("list after delete");
        assert_eq!(streams.len(), 0);
    }

    #[tokio::test]
    async fn filter_stream_cascades_on_parent_delete() {
        let (pool, _file) = test_pool().await;

        let qid = upsert_query(&pool, "repo:owner/r is:pr", "pull_request", None)
            .await
            .expect("upsert query");
        upsert_filter_stream(&pool, qid, "Open", "state:open")
            .await
            .expect("upsert filter stream");

        delete_query(&pool, qid).await.expect("delete query");

        let streams = list_filter_streams(&pool, qid)
            .await
            .expect("list after parent delete");
        assert_eq!(
            streams.len(),
            0,
            "filter streams should cascade with parent query"
        );
    }

    #[tokio::test]
    async fn cache_staleness() {
        let (pool, _file) = test_pool().await;

        let qid = upsert_query(&pool, "repo:owner/r is:open", "issue", None)
            .await
            .expect("upsert query");

        // Not yet fetched → always stale.
        let stale = is_cache_stale(&pool, qid, 300).await.expect("stale check");
        assert!(stale);

        // An *incremental* mark (`full_fetch: false`) still refreshes the cache
        // timestamp — the two columns move independently.
        mark_fetched(&pool, qid, false).await.expect("mark fetched");

        // Just fetched → not stale within 5 minutes.
        let stale = is_cache_stale(&pool, qid, 300).await.expect("stale check");
        assert!(!stale);
    }

    #[tokio::test]
    async fn full_fetch_due_until_marked_full() {
        let (pool, _file) = test_pool().await;
        let qid = upsert_query(&pool, "repo:owner/r is:open", "issue", None)
            .await
            .expect("upsert query");

        // Never full fetched → due.
        assert!(is_full_fetch_due(&pool, qid, 300).await.expect("due check"));

        // An incremental sync must not satisfy the full-fetch deadline, or ghosts
        // would never be pruned.
        mark_fetched(&pool, qid, false).await.expect("mark fetched");
        assert!(is_full_fetch_due(&pool, qid, 300).await.expect("due check"));

        mark_fetched(&pool, qid, true).await.expect("mark fetched");
        assert!(!is_full_fetch_due(&pool, qid, 300).await.expect("due check"));

        // A negative threshold makes the age comparison true without waiting or
        // time-travelling the clock: "older than -1 seconds" is always the case.
        assert!(is_full_fetch_due(&pool, qid, -1).await.expect("due check"));
    }

    #[tokio::test]
    async fn updated_since_none_until_fetched_then_rfc3339() {
        let (pool, _file) = test_pool().await;
        let qid = upsert_query(&pool, "repo:owner/r is:open", "issue", None)
            .await
            .expect("upsert query");

        // Never fetched → None (caller does a full fetch).
        assert_eq!(updated_since(&pool, qid, 600).await.expect("since"), None);

        mark_fetched(&pool, qid, false).await.expect("mark fetched");
        let since = updated_since(&pool, qid, 600)
            .await
            .expect("since")
            .expect("some after fetch");
        // RFC3339 UTC shape: "YYYY-MM-DDTHH:MM:SSZ".
        assert_eq!(since.len(), 20, "{since}");
        assert!(since.contains('T') && since.ends_with('Z'), "{since}");
    }

    #[tokio::test]
    async fn prune_removes_only_items_absent_from_keep() {
        let (pool, _file) = test_pool().await;
        let qid = upsert_query(&pool, "repo:owner/r is:pr", "pull_request", None)
            .await
            .expect("upsert query");
        for n in 1..=3 {
            upsert_item(&pool, &make_item(qid, n, &format!("PR {n}")))
                .await
                .expect("upsert");
        }

        // Keep only #1 and #3 (make_item uses owner="owner", repo="repo").
        let keep = vec![
            ("owner".to_string(), "repo".to_string(), 1),
            ("owner".to_string(), "repo".to_string(), 3),
        ];
        let deleted = prune_query_items(&pool, qid, &keep).await.expect("prune");
        assert_eq!(deleted, 1);

        let mut remaining: Vec<i64> = fetch_items(&pool, qid)
            .await
            .expect("fetch")
            .into_iter()
            .map(|i| i.number)
            .collect();
        remaining.sort();
        assert_eq!(remaining, vec![1, 3]);
    }

    #[tokio::test]
    async fn prune_with_empty_keep_deletes_all() {
        let (pool, _file) = test_pool().await;
        let qid = upsert_query(&pool, "repo:owner/r is:pr", "pull_request", None)
            .await
            .expect("upsert query");
        upsert_item(&pool, &make_item(qid, 1, "PR 1"))
            .await
            .expect("upsert");

        let deleted = prune_query_items(&pool, qid, &[]).await.expect("prune");
        assert_eq!(deleted, 1);
        assert_eq!(fetch_items(&pool, qid).await.expect("fetch").len(), 0);
    }

    #[tokio::test]
    async fn upsert_query_stores_and_returns_name() {
        let (pool, _file) = test_pool().await;

        // With a name
        let id = upsert_query(&pool, "is:pr is:open", "pull_request", Some("My PRs"))
            .await
            .expect("upsert");
        let rows = list_queries(&pool).await.expect("list");
        let row = rows.iter().find(|r| r.id == id).unwrap();
        assert_eq!(row.name.as_deref(), Some("My PRs"));
        assert_eq!(row.query, "is:pr is:open");
    }

    #[tokio::test]
    async fn upsert_query_none_name_is_null() {
        let (pool, _file) = test_pool().await;

        let id = upsert_query(&pool, "is:issue is:open", "issue", None)
            .await
            .expect("upsert");
        let rows = list_queries(&pool).await.expect("list");
        let row = rows.iter().find(|r| r.id == id).unwrap();
        assert!(row.name.is_none());
    }

    #[tokio::test]
    async fn upsert_query_empty_string_name_treated_as_null() {
        let (pool, _file) = test_pool().await;

        let id = upsert_query(&pool, "is:issue is:closed", "issue", Some(""))
            .await
            .expect("upsert");
        let rows = list_queries(&pool).await.expect("list");
        let row = rows.iter().find(|r| r.id == id).unwrap();
        // Empty string name is normalised to None.
        assert!(row.name.is_none());
    }

    #[tokio::test]
    async fn update_query_changes_fields_and_resets_fetched() {
        let (pool, _file) = test_pool().await;

        let id = upsert_query(&pool, "is:pr is:open", "pull_request", None)
            .await
            .expect("upsert");
        mark_fetched(&pool, id, true).await.expect("mark fetched");

        // Confirm not stale right after fetch.
        assert!(!is_cache_stale(&pool, id, 300).await.unwrap());
        assert!(!is_full_fetch_due(&pool, id, 300).await.unwrap());

        update_query(&pool, id, Some("Updated name"), "is:pr is:merged")
            .await
            .expect("update");

        let rows = list_queries(&pool).await.expect("list");
        let row = rows.iter().find(|r| r.id == id).unwrap();
        assert_eq!(row.query, "is:pr is:merged");
        assert_eq!(row.name.as_deref(), Some("Updated name"));

        // Both timestamps should have been reset → stale, and due for a full fetch
        // so items the *old* query matched get pruned.
        assert!(is_cache_stale(&pool, id, 300).await.unwrap());
        assert!(is_full_fetch_due(&pool, id, 300).await.unwrap());
    }

    #[tokio::test]
    async fn update_filter_stream_changes_fields() {
        let (pool, _file) = test_pool().await;

        let qid = upsert_query(&pool, "is:pr", "pull_request", None)
            .await
            .expect("upsert query");
        let fid = upsert_filter_stream(&pool, qid, "Old name", "state:open")
            .await
            .expect("upsert stream");

        update_filter_stream(&pool, fid, "New name", "state:merged")
            .await
            .expect("update");

        let streams = list_filter_streams(&pool, qid).await.expect("list");
        let stream = streams.iter().find(|s| s.id == fid).unwrap();
        assert_eq!(stream.name, "New name");
        assert_eq!(stream.filter, "state:merged");
    }

    #[tokio::test]
    async fn fetch_items_ordered_by_updated_at_desc() {
        let (pool, _file) = test_pool().await;

        let qid = upsert_query(&pool, "is:pr", "pull_request", None)
            .await
            .expect("upsert query");

        // Insert items with different updated_at values.
        let mut old = make_item(qid, 1, "Old PR");
        old.updated_at = "2026-01-01T00:00:00Z".into();
        let mut mid = make_item(qid, 2, "Mid PR");
        mid.updated_at = "2026-03-01T00:00:00Z".into();
        let mut new = make_item(qid, 3, "New PR");
        new.updated_at = "2026-05-01T00:00:00Z".into();

        upsert_item(&pool, &old).await.expect("upsert old");
        upsert_item(&pool, &mid).await.expect("upsert mid");
        upsert_item(&pool, &new).await.expect("upsert new");

        let items = fetch_items(&pool, qid).await.expect("fetch");
        assert_eq!(items.len(), 3);
        // Should be in descending order by updated_at.
        assert_eq!(items[0].number, 3); // newest
        assert_eq!(items[1].number, 2);
        assert_eq!(items[2].number, 1); // oldest
    }

    #[tokio::test]
    async fn clear_stale_bodies_clears_terminal_and_old_but_keeps_recent_and_unread_state() {
        let (pool, _file) = test_pool().await;
        let qid = upsert_query(&pool, "repo:owner/r is:pr", "pull_request", None)
            .await
            .expect("upsert query");

        // Recent open item: body kept. Far-future date is always within retention.
        let mut recent = make_item(qid, 1, "Recent open");
        recent.updated_at = "2999-01-01T00:00:00Z".into();
        recent.body = Some("recent body".into());
        // Old open item: body cleared by age.
        let mut old = make_item(qid, 2, "Old open");
        old.updated_at = "2000-01-01T00:00:00Z".into();
        old.body = Some("old body".into());
        // Recent but terminal item: body cleared by state.
        let mut closed = make_item(qid, 3, "Recent closed");
        closed.updated_at = "2999-01-01T00:00:00Z".into();
        closed.state = "closed".into();
        closed.body = Some("closed body".into());

        for it in [&recent, &old, &closed] {
            upsert_item(&pool, it).await.expect("upsert");
        }
        // Read state on the recent item, to prove it survives the body clear.
        mark_item_read(&pool, qid, "owner", "repo", 1)
            .await
            .expect("mark read");

        let cleared = clear_stale_bodies(&pool, 90).await.expect("clear");
        assert_eq!(cleared, 2, "old-open and closed bodies cleared");

        let by_num: std::collections::HashMap<i64, CachedItem> = fetch_items(&pool, qid)
            .await
            .expect("fetch")
            .into_iter()
            .map(|i| (i.number, i))
            .collect();
        assert_eq!(by_num[&1].body.as_deref(), Some("recent body"));
        assert_eq!(by_num[&2].body, None);
        assert_eq!(by_num[&3].body, None);
        // Unread marker untouched by the body clear.
        assert_eq!(
            by_num[&1].last_read_updated_at.as_deref(),
            Some("2999-01-01T00:00:00Z")
        );
    }

    #[tokio::test]
    async fn prune_query_overflow_deletes_read_overflow_only() {
        let (pool, _file) = test_pool().await;
        let qid = upsert_query(&pool, "repo:owner/r is:pr", "pull_request", None)
            .await
            .expect("upsert query");

        // Five items, ascending updated_at so #5 is newest, #1 oldest.
        for n in 1..=5 {
            let mut it = make_item(qid, n, &format!("PR {n}"));
            it.updated_at = format!("2026-0{n}-01T00:00:00Z");
            upsert_item(&pool, &it).await.expect("upsert");
        }
        // Overflow beyond newest 2 is {1,2,3}. Mark 1 and 2 read; leave 3 unread.
        mark_item_read(&pool, qid, "owner", "repo", 1)
            .await
            .expect("read 1");
        mark_item_read(&pool, qid, "owner", "repo", 2)
            .await
            .expect("read 2");

        let deleted = prune_query_overflow(&pool, qid, 2).await.expect("prune");
        assert_eq!(deleted, 2, "only the two read overflow rows are deleted");

        let mut nums: Vec<i64> = fetch_items(&pool, qid)
            .await
            .expect("fetch")
            .into_iter()
            .map(|i| i.number)
            .collect();
        nums.sort();
        // #3 survives despite being overflow, because it is unread.
        assert_eq!(nums, vec![3, 4, 5]);
    }
}
