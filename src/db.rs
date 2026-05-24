use anyhow::Result;
use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};
use std::path::PathBuf;

pub async fn open_pool(db_path: &PathBuf) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true);
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
pub async fn swap_filter_stream_positions(pool: &SqlitePool, id1: i64, id2: i64) -> Result<()> {
    let pos1 = sqlx::query_scalar!("SELECT position FROM filter_streams WHERE id = ?", id1)
        .fetch_one(pool)
        .await?;
    let pos2 = sqlx::query_scalar!("SELECT position FROM filter_streams WHERE id = ?", id2)
        .fetch_one(pool)
        .await?;
    sqlx::query!(
        "UPDATE filter_streams SET position = ? WHERE id = ?",
        pos2,
        id1
    )
    .execute(pool)
    .await?;
    sqlx::query!(
        "UPDATE filter_streams SET position = ? WHERE id = ?",
        pos1,
        id2
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Update an existing query's display name and/or search string.
/// Passing `None` for `name` clears the display name (falls back to query string).
/// Resets last_fetched_at so the cache is considered stale.
pub async fn update_query(
    pool: &SqlitePool,
    id: i64,
    name: Option<&str>,
    query: &str,
) -> Result<()> {
    sqlx::query!(
        "UPDATE queries SET name = ?, query = ?, last_fetched_at = NULL WHERE id = ?",
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
pub async fn swap_query_positions(pool: &SqlitePool, id1: i64, id2: i64) -> Result<()> {
    let pos1 = sqlx::query_scalar!("SELECT position FROM queries WHERE id = ?", id1)
        .fetch_one(pool)
        .await?;
    let pos2 = sqlx::query_scalar!("SELECT position FROM queries WHERE id = ?", id2)
        .fetch_one(pool)
        .await?;
    sqlx::query!("UPDATE queries SET position = ? WHERE id = ?", pos2, id1)
        .execute(pool)
        .await?;
    sqlx::query!("UPDATE queries SET position = ? WHERE id = ?", pos1, id2)
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

/// Mark a query as freshly fetched.
pub async fn mark_fetched(pool: &SqlitePool, query_id: i64) -> Result<()> {
    sqlx::query!(
        "UPDATE queries SET last_fetched_at = datetime('now') WHERE id = ?",
        query_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub struct CachedItem {
    pub query_id: i64,
    pub kind: String,
    pub repo_owner: String,
    pub repo_name: String,
    pub number: i64,
    pub title: String,
    pub url: String,
    pub author: Option<String>,
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
}

/// Insert or replace a cached item for a query.
pub async fn upsert_item(pool: &SqlitePool, item: &CachedItem) -> Result<()> {
    let is_draft_int = item.is_draft as i64;
    sqlx::query!(
        r#"
        INSERT INTO items
            (query_id, kind, repo_owner, repo_name, number, title, url, author,
             state, updated_at, labels, comment_count, requested_reviewers, reviews,
             body, assignees, is_draft, created_at_item, base_ref, head_ref,
             review_decision, milestone, cached_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'))
        ON CONFLICT (query_id, repo_owner, repo_name, number)
        DO UPDATE SET
            title               = excluded.title,
            url                 = excluded.url,
            author              = excluded.author,
            state               = excluded.state,
            updated_at          = excluded.updated_at,
            labels              = excluded.labels,
            comment_count       = excluded.comment_count,
            requested_reviewers = excluded.requested_reviewers,
            reviews             = excluded.reviews,
            body                = excluded.body,
            assignees           = excluded.assignees,
            is_draft            = excluded.is_draft,
            created_at_item     = excluded.created_at_item,
            base_ref            = excluded.base_ref,
            head_ref            = excluded.head_ref,
            review_decision     = excluded.review_decision,
            milestone           = excluded.milestone,
            cached_at           = excluded.cached_at
        "#,
        item.query_id,
        item.kind,
        item.repo_owner,
        item.repo_name,
        item.number,
        item.title,
        item.url,
        item.author,
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
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch all cached items for a query.
pub async fn fetch_items(pool: &SqlitePool, query_id: i64) -> Result<Vec<CachedItem>> {
    let rows = sqlx::query!(
        r#"
        SELECT query_id, kind, repo_owner, repo_name, number, title, url, author,
               state, updated_at, labels, comment_count, requested_reviewers, reviews,
               body, assignees, is_draft, created_at_item, base_ref, head_ref,
               review_decision, milestone
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
            number: r.number,
            title: r.title,
            url: r.url,
            author: r.author,
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
        })
        .collect())
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
        Some(ts) => {
            let stale: bool = sqlx::query_scalar!(
                r#"SELECT (strftime('%s', 'now') - strftime('%s', ?)) > ? AS "stale: bool""#,
                ts,
                max_age_secs,
            )
            .fetch_one(pool)
            .await?;
            Ok(stale)
        }
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
            .expect("open pool");
        (pool, file)
    }

    fn make_item(query_id: i64, number: i64, title: &str) -> CachedItem {
        CachedItem {
            query_id,
            kind: "pull_request".into(),
            repo_owner: "owner".into(),
            repo_name: "repo".into(),
            number,
            title: title.to_string(),
            url: format!("https://github.com/owner/repo/pull/{number}"),
            author: Some("alice".into()),
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

        let item = CachedItem {
            query_id: qid,
            kind: "pull_request".into(),
            repo_owner: "owner".into(),
            repo_name: "r".into(),
            number: 1,
            title: "Fix bug".into(),
            url: "https://github.com/owner/r/pull/1".into(),
            author: Some("alice".into()),
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
        };
        upsert_item(&pool, &item).await.expect("upsert item");

        let items = fetch_items(&pool, qid).await.expect("fetch items");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Fix bug");
        assert_eq!(items[0].author.as_deref(), Some("alice"));
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

        mark_fetched(&pool, qid).await.expect("mark fetched");

        // Just fetched → not stale within 5 minutes.
        let stale = is_cache_stale(&pool, qid, 300).await.expect("stale check");
        assert!(!stale);
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
        mark_fetched(&pool, id).await.expect("mark fetched");

        // Confirm not stale right after fetch.
        assert!(!is_cache_stale(&pool, id, 300).await.unwrap());

        update_query(&pool, id, Some("Updated name"), "is:pr is:merged")
            .await
            .expect("update");

        let rows = list_queries(&pool).await.expect("list");
        let row = rows.iter().find(|r| r.id == id).unwrap();
        assert_eq!(row.query, "is:pr is:merged");
        assert_eq!(row.name.as_deref(), Some("Updated name"));

        // last_fetched_at should have been reset → stale again.
        assert!(is_cache_stale(&pool, id, 300).await.unwrap());
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
}
