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

/// List all saved queries ordered by creation time.
pub async fn list_queries(pool: &SqlitePool) -> Result<Vec<QueryRecord>> {
    let rows = sqlx::query!(
        "SELECT id, query, kind, name FROM queries ORDER BY created_at ASC"
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

/// List filter streams for a given parent query, ordered by creation time.
pub async fn list_filter_streams(
    pool: &SqlitePool,
    parent_id: i64,
) -> Result<Vec<FilterStreamRecord>> {
    let rows = sqlx::query!(
        "SELECT id, parent_id, name, filter FROM filter_streams WHERE parent_id = ? ORDER BY created_at ASC",
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

/// Upsert a query record and return its id.
pub async fn upsert_query(
    pool: &SqlitePool,
    query: &str,
    kind: &str,
) -> Result<i64> {
    let row = sqlx::query!(
        r#"
        INSERT INTO queries (query, kind)
        VALUES (?, ?)
        ON CONFLICT (query) DO UPDATE SET query = excluded.query
        RETURNING id
        "#,
        query,
        kind,
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
}

/// Insert or replace a cached item for a query.
pub async fn upsert_item(pool: &SqlitePool, item: &CachedItem) -> Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO items
            (query_id, kind, repo_owner, repo_name, number, title, url, author,
             state, updated_at, labels, comment_count, cached_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'))
        ON CONFLICT (query_id, repo_owner, repo_name, number)
        DO UPDATE SET
            title         = excluded.title,
            url           = excluded.url,
            author        = excluded.author,
            state         = excluded.state,
            updated_at    = excluded.updated_at,
            labels        = excluded.labels,
            comment_count = excluded.comment_count,
            cached_at     = excluded.cached_at
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
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch all cached items for a query.
pub async fn fetch_items(
    pool: &SqlitePool,
    query_id: i64,
) -> Result<Vec<CachedItem>> {
    let rows = sqlx::query!(
        r#"
        SELECT query_id, kind, repo_owner, repo_name, number, title, url, author,
               state, updated_at, labels, comment_count
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
        })
        .collect())
}

/// Check whether the cache for a query is stale (older than `max_age_secs`).
pub async fn is_cache_stale(
    pool: &SqlitePool,
    query_id: i64,
    max_age_secs: i64,
) -> Result<bool> {
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
        }
    }

    #[tokio::test]
    async fn delete_query_cascades_items() {
        let (pool, _file) = test_pool().await;

        let qid = upsert_query(&pool, "repo:owner/r is:pr", "pull_request")
            .await
            .expect("upsert query");

        upsert_item(&pool, &make_item(qid, 1, "First")).await.expect("upsert");
        upsert_item(&pool, &make_item(qid, 2, "Second")).await.expect("upsert");

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

        upsert_query(&pool, "query:first", "issue").await.expect("upsert");
        upsert_query(&pool, "query:second", "pull_request").await.expect("upsert");
        upsert_query(&pool, "query:third", "issue").await.expect("upsert");

        let queries = list_queries(&pool).await.expect("list");
        assert_eq!(queries.len(), 3);
        assert_eq!(queries[0].query, "query:first");
        assert_eq!(queries[1].query, "query:second");
        assert_eq!(queries[2].query, "query:third");
    }

    #[tokio::test]
    async fn upsert_item_updates_on_conflict() {
        let (pool, _file) = test_pool().await;

        let qid = upsert_query(&pool, "repo:owner/r is:pr", "pull_request")
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

        let qid = upsert_query(&pool, "repo:owner/r is:pr", "pull_request")
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

        let qid = upsert_query(&pool, "repo:owner/r is:pr", "pull_request")
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
        let streams = list_filter_streams(&pool, qid).await.expect("list after delete");
        assert_eq!(streams.len(), 0);
    }

    #[tokio::test]
    async fn filter_stream_cascades_on_parent_delete() {
        let (pool, _file) = test_pool().await;

        let qid = upsert_query(&pool, "repo:owner/r is:pr", "pull_request")
            .await
            .expect("upsert query");
        upsert_filter_stream(&pool, qid, "Open", "state:open")
            .await
            .expect("upsert filter stream");

        delete_query(&pool, qid).await.expect("delete query");

        let streams = list_filter_streams(&pool, qid).await.expect("list after parent delete");
        assert_eq!(streams.len(), 0, "filter streams should cascade with parent query");
    }

    #[tokio::test]
    async fn cache_staleness() {
        let (pool, _file) = test_pool().await;

        let qid = upsert_query(&pool, "repo:owner/r is:open", "issue")
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
}
