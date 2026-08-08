//! Shared fixtures for `glauca-core`'s unit tests.
//!
//! `db` and `engine` both need a migrated throwaway cache and a `CachedItem` to put in it.
//! One copy here means a new `items` column is added to one builder rather than several,
//! and the two layers' tests can't drift into exercising differently-shaped rows.

use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;
use std::path::Path;
use tempfile::NamedTempFile;

use crate::db::{CachedItem, open_pool};

/// A fresh migrated cache backed by a temp file. The returned `NamedTempFile` must
/// be kept alive for the pool's lifetime — dropping it deletes the database.
pub async fn test_pool() -> (SqlitePool, NamedTempFile) {
    let file = NamedTempFile::new().expect("tempfile");
    let pool = open_pool(file.path())
        .await
        .unwrap_or_else(|e| panic!("open pool: {e:#}"));
    (pool, file)
}

/// A connection to `path` that applies no migrations and no pragmas — for tests that need
/// to set up or inspect a database *without* going through `open_pool`.
pub async fn raw_pool(path: &Path) -> SqlitePool {
    SqlitePool::connect_with(SqliteConnectOptions::new().filename(path))
        .await
        .unwrap_or_else(|e| panic!("connect to {}: {e:#}", path.display()))
}

/// A SQLite file that is not a glauca cache: it holds `table_sql` and no
/// `_sqlx_migrations`, which is how `open_pool` tells someone else's database from ours.
pub async fn foreign_database(table_sql: &str) -> NamedTempFile {
    let file = NamedTempFile::new().expect("tempfile");
    let pool = raw_pool(file.path()).await;
    sqlx::query(table_sql)
        .execute(&pool)
        .await
        .unwrap_or_else(|e| panic!("run {table_sql}: {e:#}"));
    pool.close().await;
    file
}

/// A minimal open pull request in `owner/repo`, identified by `number`.
pub fn make_item(query_id: i64, number: i64, title: &str) -> CachedItem {
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
