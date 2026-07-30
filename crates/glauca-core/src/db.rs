use anyhow::Result;
use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};
use std::path::PathBuf;
use std::time::Duration;
use tracing::debug;

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
///
/// When the *search string* actually changed, resets both fetch timestamps — the cache
/// is stale, and the edited query needs a fresh full fetch to prune items the *old*
/// query matched but the new one doesn't — and arms every cached row one strike short
/// of deletion, so that first fetch drops whatever it no longer returns instead of
/// making the user stare at the old result set for another full-fetch interval.
///
/// Renames must not do any of that: the result set is unchanged, so the cache is
/// exactly as fresh as it was, and the transient absences corroboration exists to
/// absorb (pagination races, search-index lag) are fully live — a single one would
/// cost a live row its read marker. The front-ends submit name and query together
/// from one form, so telling the two cases apart has to happen here.
pub async fn update_query(
    pool: &SqlitePool,
    id: i64,
    name: Option<&str>,
    query: &str,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let previous = sqlx::query_scalar!("SELECT query FROM queries WHERE id = ?", id)
        .fetch_optional(&mut *tx)
        .await?;
    let query_changed = previous.as_deref() != Some(query);

    if query_changed {
        sqlx::query!(
            "UPDATE queries SET name = ?, query = ?, last_fetched_at = NULL, last_full_fetch_at = NULL, last_full_fetch_attempt_at = NULL WHERE id = ?",
            name,
            query,
            id,
        )
        .execute(&mut *tx)
        .await?;
        // Rows the new query does return are reset to 0 by `upsert_items`.
        let armed_missing_count = PRUNE_STRIKES - 1;
        sqlx::query!(
            "UPDATE items SET missing_count = ? WHERE query_id = ?",
            armed_missing_count,
            id
        )
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query!("UPDATE queries SET name = ? WHERE id = ?", name, id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
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
/// `last_full_fetch_at` — the promise that this walk covered the whole result set
/// *and* pruned (or couldn't) — and `last_full_fetch_attempt_at`, which is what defers
/// the next full walk. Pass `true` only when both hold.
///
/// This is the only writer of `last_full_fetch_at`, and it writes it in the same
/// statement as `last_fetched_at`, so `last_full_fetch_at <= last_fetched_at` always
/// holds. `mark_full_fetch_attempted` records a *failed* walk and touches only the
/// attempt column, which is therefore always the later of the two.
pub async fn mark_fetched(pool: &SqlitePool, query_id: i64, full_fetch: bool) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE queries
        SET last_fetched_at            = datetime('now'),
            last_full_fetch_at         = CASE WHEN ? THEN datetime('now') ELSE last_full_fetch_at END,
            last_full_fetch_attempt_at = CASE WHEN ? THEN datetime('now') ELSE last_full_fetch_attempt_at END
        WHERE id = ?
        "#,
        full_fetch,
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

/// Item identity within a query's cache: (repo_owner, repo_name, number).
pub type ItemKey = (String, String, i64);

/// How many consecutive full fetches must fail to return an item before it is
/// deleted. Two means "missing twice in a row"; see the `missing_count` migration for
/// why one absence isn't proof.
pub const PRUNE_STRIKES: i64 = 2;

/// Record that this full fetch didn't return the cached rows absent from `keep`, and
/// delete the ones that have reached `strikes_required` consecutive absences. Returns
/// the number of rows deleted.
///
/// `upsert_items` zeroes the counter, so any search that returns the item again
/// disarms it, and the threshold is only ever reached by *consecutive* absences.
/// `strikes_required` comes from `engine::PruneTrust`.
///
/// Read state is lost on deletion: `last_read_updated_at` lives on the row, and a
/// re-insert always arrives with it unset (see `upsert_item`), so an item that leaves a
/// query and later matches again comes back as unread. Unlike `prune_query_overflow` —
/// which reasons at length about avoiding exactly that, and protects the newest rows —
/// pruning has no such protection, because deleting rows that no longer match is its
/// whole purpose. The behaviour is intended: a re-requested review or a reopened issue
/// is new actionable work, so surfacing it as unread is correct. What is *not* intended
/// is deleting a row that still matches, which is what the strike count guards against.
///
/// `last_full_fetch_before_walk` is `last_full_fetch_at` as read *before* this walk began. If it has
/// moved by now, a concurrent full fetch finished first and this walk's absences are
/// not an independent observation — counting them would let two overlapping walks land
/// both strikes against one transient, deleting a live row. Nothing is pruned then.
/// (Foreground `Sync`/`SyncIfStale` don't go through `SyncCoalescer`, so they really can
/// overlap a background sync of the same query.)
///
/// TODO: fold the stamp check and the stamp write into one transaction (check-and-claim
/// here, demoting `mark_fetched` to `last_fetched_at`). The current check narrows the
/// race rather than closing it: `sync_task` stamps *after* this returns, so a second
/// walk committing inside that gap still sees the old value; `datetime('now')` has
/// one-second resolution; and two walks predating a query's first completed full fetch
/// both observe `NULL`. Moving the write is not enough on its own — `sync_task` also
/// stamps on the paths where `may_prune` is false and this function never runs, so
/// those need a second stamping route. Deferred because each window needs overlapping
/// full walks *and* a transiently-absent row, and costs one row's read marker when it
/// fires.
///
/// Caller must only pass `keep` from an untruncated, complete full fetch — see
/// `engine::may_prune`.
pub async fn prune_missing_items(
    pool: &SqlitePool,
    query_id: i64,
    keep: &[ItemKey],
    strikes_required: i64,
    last_full_fetch_before_walk: Option<&str>,
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

    let missing_ids: Vec<i64> = existing
        .into_iter()
        .filter(|r| !keep_set.contains(&(r.repo_owner.as_str(), r.repo_name.as_str(), r.number)))
        .map(|r| r.id)
        .collect();
    if missing_ids.is_empty() {
        return Ok(0);
    }

    // Increment then delete in one transaction, so a crash between the two can't
    // leave a strike recorded against a row that was about to be deleted anyway
    // (harmless) or, worse, delete without having counted (impossible here). Reading
    // the stamp inside the same transaction is what makes the concurrency check
    // meaningful: the winning walk's `mark_fetched` can't land between our read and
    // our writes.
    //
    // BEGIN IMMEDIATE, not the default deferred BEGIN: this transaction reads before
    // it writes, and SQLite answers a SHARED→RESERVED promotion with an instant
    // SQLITE_BUSY *without consulting the busy handler* (waiting there could
    // deadlock), so `busy_timeout` would not save us from a concurrent writer —
    // another query's `upsert_items`, a read-marking update, or the maintenance
    // sweep. Taking the write lock up front makes the 30s timeout apply as intended.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if last_full_fetch_at(&mut *tx, query_id).await?.as_deref() != last_full_fetch_before_walk {
        debug!("skipping prune: a concurrent full fetch finished first");
        tx.rollback().await?;
        return Ok(0);
    }

    // Bind the id list once as JSON and let SQLite's json_each expand it, rather than
    // building `IN (?, ?, …)` by hand: that would need chunking under the
    // bound-variable limit, and a hand-built `QueryBuilder` opts out of sqlx's
    // compile-time checking for the two statements that do the actual deleting.
    let ids = serde_json::to_string(&missing_ids)?;
    sqlx::query!(
        r#"
        UPDATE items SET missing_count = missing_count + 1
        WHERE id IN (SELECT value FROM json_each(?))
        "#,
        ids,
    )
    .execute(&mut *tx)
    .await?;
    let deleted = sqlx::query!(
        r#"
        DELETE FROM items
        WHERE missing_count >= ? AND id IN (SELECT value FROM json_each(?))
        "#,
        strikes_required,
        ids,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();

    tx.commit().await?;
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

/// Insert or replace one cached item, out of band from a search.
///
/// Does *not* clear the item's prune strikes: `github::fetch_item` looks an item up by
/// repo and number and says nothing about whether it still matches the query, so this
/// is no evidence of membership. Front-ends call it automatically to re-fetch a
/// maintenance-cleared body, and letting that disarm a ghost the user merely clicked on
/// would keep it alive for another full-fetch interval.
pub async fn upsert_item(pool: &SqlitePool, item: &CachedItem) -> Result<()> {
    upsert_item_with(pool, item, false).await
}

/// Insert or replace a whole page of *search results* in one transaction, clearing
/// each item's prune strikes — the query returned them, so they still match.
///
/// Each `upsert_item` is otherwise its own implicit transaction, and SQLite's default
/// `synchronous=FULL` makes that a couple of fsyncs apiece. That was tolerable when a
/// sync only wrote the handful of items whose `updated_at` had moved, but a periodic
/// full fetch re-upserts a query's entire result set — up to `SEARCH_RESULT_CAP` rows
/// — so the per-item commits turn into a visible stall on the same file the UI reads
/// from. One transaction per page also makes the page atomic: a mid-page failure no
/// longer leaves half of it applied.
pub async fn upsert_items(pool: &SqlitePool, items: &[CachedItem]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for item in items {
        upsert_item_with(&mut *tx, item, true).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The upsert statement itself, generic over pool/transaction so the single-item and
/// batched entry points can't drift apart. `matched_query` clears `missing_count`.
async fn upsert_item_with<'e, E>(exec: E, item: &CachedItem, matched_query: bool) -> Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
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
            milestone           = excluded.milestone,
            -- A search that returned this item proves it still matches, so clear the
            -- prune strikes it may have accumulated (see `prune_missing_items`). A
            -- by-number re-fetch proves nothing about membership, so it leaves them.
            missing_count       = CASE WHEN ? THEN 0 ELSE missing_count END
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
        matched_query,
    )
    .execute(exec)
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

/// When `query_id` was last full fetched, or `None` if never.
///
/// Generic over the executor because [`prune_missing_items`] must re-read this *inside*
/// its transaction for the concurrency check to mean anything, while everyone else
/// reads it from the pool.
pub async fn last_full_fetch_at<'e, E>(exec: E, query_id: i64) -> Result<Option<String>>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row = sqlx::query!(
        r#"SELECT last_full_fetch_at FROM queries WHERE id = ?"#,
        query_id,
    )
    .fetch_one(exec)
    .await?;
    Ok(row.last_full_fetch_at)
}

/// Stamp `last_full_fetch_at` without touching `last_fetched_at`: a full walk was
/// attempted and failed.
///
/// Leaving the stamp alone would promote *every* subsequent sync to a full re-page for
/// as long as the failure lasts — a query that reliably errors on page 3 would walk
/// three pages a minute forever, with no backoff on the non-rate-limited error path.
/// `last_fetched_at` is deliberately left stale so the query is still retried next
/// cycle; only the expensive *full* walk is deferred.
///
/// The deferral only helps a query that completed a fetch at least once before it
/// started failing. One that has never succeeded has `last_fetched_at = NULL`, so
/// `updated_since` yields nothing to be incremental against and `resolve_since` must
/// keep choosing a full fetch regardless — there is no cheaper retry to fall back to.
/// Editing a query re-enters that state.
///
/// Only the attempt column is written: `last_full_fetch_at` means "a full walk
/// completed" and is the prune concurrency guard's comparison value, so a failure
/// moving it would make a *concurrent* successful walk mistake this for the winner and
/// skip its prune. See the `20260730000002` migration.
pub async fn mark_full_fetch_attempted(pool: &SqlitePool, query_id: i64) -> Result<()> {
    sqlx::query!(
        "UPDATE queries SET last_full_fetch_attempt_at = datetime('now') WHERE id = ?",
        query_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether `query_id` is due for a *full* (non-incremental) fetch: no full walk has
/// ever been attempted (NULL) or the last attempt is older than `max_age_secs`.
///
/// Reads the *attempt* stamp, not the completion stamp: a query whose full walk keeps
/// failing must not re-page on every sync (see `mark_full_fetch_attempted`). Because
/// `mark_fetched` stamps both, a completed walk defers the retry just the same.
///
/// Only a full fetch is an authoritative result set, so only a full fetch may prune
/// rows that left the query ([`prune_missing_items`]). This is therefore what bounds
/// how long a stale row can linger; see `engine::resolve_since`.
pub async fn is_full_fetch_due(
    pool: &SqlitePool,
    query_id: i64,
    max_age_secs: i64,
) -> Result<bool> {
    let row = sqlx::query!(
        r#"SELECT last_full_fetch_attempt_at FROM queries WHERE id = ?"#,
        query_id,
    )
    .fetch_one(pool)
    .await?;
    match row.last_full_fetch_attempt_at {
        None => Ok(true),
        Some(ts) => older_than(pool, &ts, max_age_secs).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{make_item, test_pool};

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

        assert!(
            is_full_fetch_due(&pool, qid, 300).await.expect("due check"),
            "a query that was never full fetched is due"
        );

        mark_fetched(&pool, qid, false).await.expect("mark fetched");
        assert!(
            is_full_fetch_due(&pool, qid, 300).await.expect("due check"),
            "an incremental sync must not satisfy the full-fetch deadline, or ghosts \
             would never be pruned"
        );

        mark_fetched(&pool, qid, true).await.expect("mark fetched");
        assert!(
            !is_full_fetch_due(&pool, qid, 300).await.expect("due check"),
            "a full fetch satisfies it"
        );
    }

    #[tokio::test]
    async fn full_fetch_due_again_once_the_interval_passes() {
        let (pool, _file) = test_pool().await;
        let qid = upsert_query(&pool, "repo:owner/r is:open", "issue", None)
            .await
            .expect("upsert query");
        mark_fetched(&pool, qid, true).await.expect("mark fetched");

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

    /// Numbers still cached for `query_id`, sorted.
    async fn remaining_numbers(pool: &SqlitePool, query_id: i64) -> Vec<i64> {
        let mut ns: Vec<i64> = fetch_items(pool, query_id)
            .await
            .expect("fetch")
            .into_iter()
            .map(|i| i.number)
            .collect();
        ns.sort();
        ns
    }

    /// The search string `query_with_items` saves. Named so a test can say "unchanged"
    /// without duplicating the literal.
    const CACHED_QUERY: &str = "repo:owner/r is:pr";

    /// A query whose cache holds `numbers`, all with zero strikes.
    async fn query_with_items(pool: &SqlitePool, numbers: &[i64]) -> i64 {
        let qid = upsert_query(pool, CACHED_QUERY, "pull_request", None)
            .await
            .expect("upsert query");
        for n in numbers {
            upsert_item(pool, &make_item(qid, *n, &format!("PR {n}")))
                .await
                .expect("upsert");
        }
        qid
    }

    fn keep(numbers: &[i64]) -> Vec<ItemKey> {
        numbers
            .iter()
            .map(|n| ("owner".to_string(), "repo".to_string(), *n))
            .collect()
    }

    /// The next search returned `numbers`, which clears their strikes.
    async fn search_returned(pool: &SqlitePool, query_id: i64, numbers: &[i64]) {
        let items: Vec<CachedItem> = numbers
            .iter()
            .map(|n| make_item(query_id, *n, &format!("PR {n}")))
            .collect();
        upsert_items(pool, &items).await.expect("upsert");
    }

    /// Prune as a fetch that observed `last_full_fetch_before_walk` before its walk would.
    async fn prune_observing(
        pool: &SqlitePool,
        query_id: i64,
        keep: &[ItemKey],
        last_full_fetch_before_walk: Option<&str>,
    ) -> u64 {
        prune_missing_items(
            pool,
            query_id,
            keep,
            PRUNE_STRIKES,
            last_full_fetch_before_walk,
        )
        .await
        .expect("prune")
    }

    /// An automatic sync's prune of a query that has never been full fetched (so the
    /// stamp it observed is `None`).
    async fn auto_prune(pool: &SqlitePool, query_id: i64, keep: &[ItemKey]) -> u64 {
        prune_observing(pool, query_id, keep, None).await
    }

    /// A user's explicit resync: `PruneTrust::Immediate`, i.e. delete on first absence.
    async fn forced_prune(pool: &SqlitePool, query_id: i64, keep: &[ItemKey]) -> u64 {
        prune_missing_items(pool, query_id, keep, 1, None)
            .await
            .expect("prune")
    }

    /// The corroboration rule: one absence only records a strike, the second deletes.
    /// This is what stops the pagination race (an item updated mid-walk moves past the
    /// cursor and is never returned) from destroying a live row and its read marker.
    #[tokio::test]
    async fn prune_requires_two_consecutive_absences() {
        let (pool, _file) = test_pool().await;
        let qid = query_with_items(&pool, &[1, 2, 3]).await;

        // #2 absent once → strike recorded, nothing deleted.
        assert_eq!(auto_prune(&pool, qid, &keep(&[1, 3])).await, 0);
        assert_eq!(remaining_numbers(&pool, qid).await, vec![1, 2, 3]);

        // Absent again → deleted.
        assert_eq!(auto_prune(&pool, qid, &keep(&[1, 3])).await, 1);
        assert_eq!(remaining_numbers(&pool, qid).await, vec![1, 3]);
    }

    /// A user's explicit full resync (`S`) prunes on the first absence: it's a
    /// deliberate "clean this up now", and needing to press it twice would look broken.
    #[tokio::test]
    async fn forced_resync_prunes_on_first_absence() {
        let (pool, _file) = test_pool().await;
        let qid = query_with_items(&pool, &[1, 2]).await;

        assert_eq!(forced_prune(&pool, qid, &keep(&[1])).await, 1);
        assert_eq!(remaining_numbers(&pool, qid).await, vec![1]);
    }

    /// An item that comes back must be disarmed by the next search, not deleted by a
    /// later unrelated absence. This is the property that makes the strike count safe
    /// to persist: it can only ever reach the threshold on *consecutive* misses.
    #[tokio::test]
    async fn prune_strikes_reset_when_an_item_comes_back() {
        let (pool, _file) = test_pool().await;
        let qid = query_with_items(&pool, &[1, 2]).await;

        // Strike one against #2.
        auto_prune(&pool, qid, &keep(&[1])).await;
        // #2 is returned by the next search, which resets its counter.
        search_returned(&pool, qid, &[2]).await;

        // Absent again — that's a *first* strike again, so it survives.
        assert_eq!(auto_prune(&pool, qid, &keep(&[1])).await, 0);
        assert_eq!(remaining_numbers(&pool, qid).await, vec![1, 2]);
    }

    /// A by-number re-fetch (`RefreshItem`, e.g. re-loading a cleared body) says
    /// nothing about query membership, so it must NOT disarm a ghost the user happened
    /// to click on.
    #[tokio::test]
    async fn single_item_refresh_does_not_reset_prune_strikes() {
        let (pool, _file) = test_pool().await;
        let qid = query_with_items(&pool, &[1, 2]).await;

        auto_prune(&pool, qid, &keep(&[1])).await;
        upsert_item(&pool, &make_item(qid, 2, "PR 2"))
            .await
            .expect("refresh");

        // Still the second strike → deleted.
        assert_eq!(auto_prune(&pool, qid, &keep(&[1])).await, 1);
        assert_eq!(remaining_numbers(&pool, qid).await, vec![1]);
    }

    /// Two overlapping full fetches must not land both strikes against one transient:
    /// the loser sees the stamp has moved and prunes nothing.
    #[tokio::test]
    async fn prune_skipped_when_a_concurrent_full_fetch_finished_first() {
        let (pool, _file) = test_pool().await;
        let qid = query_with_items(&pool, &[1, 2]).await;

        // Walk A observed no stamp, pruned (strike 1), and marked the query fetched.
        assert_eq!(prune_observing(&pool, qid, &keep(&[1]), None).await, 0);
        mark_fetched(&pool, qid, true).await.expect("mark");

        // Walk B started before that and also observed no stamp — its absence is not an
        // independent observation, so it must not count a second strike.
        assert_eq!(prune_observing(&pool, qid, &keep(&[1]), None).await, 0);
        assert_eq!(remaining_numbers(&pool, qid).await, vec![1, 2]);

        // A later walk that observed the current stamp counts normally.
        let stamp = last_full_fetch_at(&pool, qid).await.expect("stamp");
        assert_eq!(
            prune_observing(&pool, qid, &keep(&[1]), stamp.as_deref()).await,
            1
        );
    }

    /// Strikes live on the row, so one query's misses neither delete another query's
    /// copy of the same item nor count towards its threshold.
    #[tokio::test]
    async fn prune_strikes_are_per_row() {
        let (pool, _file) = test_pool().await;
        let qa = query_with_items(&pool, &[1]).await;
        let qb = upsert_query(&pool, "repo:owner/other is:pr", "pull_request", None)
            .await
            .expect("upsert query");
        upsert_item(&pool, &make_item(qb, 1, "PR 1"))
            .await
            .expect("upsert");

        // Two strikes against query A's copy deletes only that copy.
        for _ in 0..2 {
            auto_prune(&pool, qa, &[]).await;
        }
        assert_eq!(remaining_numbers(&pool, qa).await, Vec::<i64>::new());
        assert_eq!(remaining_numbers(&pool, qb).await, vec![1]);

        // B's row inherited none of those strikes: its own first absence is strike one.
        assert_eq!(
            auto_prune(&pool, qb, &[]).await,
            0,
            "query B's row must start from zero strikes"
        );
        assert_eq!(remaining_numbers(&pool, qb).await, vec![1]);
    }

    /// Editing a query arms its rows: the definition changed, so whatever the first
    /// fetch under the new query doesn't return is stale by construction and should go
    /// immediately rather than after another full-fetch interval.
    #[tokio::test]
    async fn editing_a_query_prunes_on_the_next_fetch() {
        let (pool, _file) = test_pool().await;
        let qid = query_with_items(&pool, &[1, 2]).await;

        update_query(&pool, qid, None, "repo:owner/r is:merged")
            .await
            .expect("update");

        // First fetch under the new definition drops what it no longer returns…
        assert_eq!(auto_prune(&pool, qid, &keep(&[1])).await, 1);
        // …but keeps what it does.
        assert_eq!(remaining_numbers(&pool, qid).await, vec![1]);
    }

    /// Renaming must NOT arm: the search string is unchanged, so the result set is too,
    /// and a single transient absence would then cost a live row its read marker. The
    /// front-ends submit name and query together, so only `update_query` can tell the
    /// two edits apart.
    #[tokio::test]
    async fn renaming_a_query_does_not_arm_prune_strikes() {
        let (pool, _file) = test_pool().await;
        let qid = query_with_items(&pool, &[1]).await;

        update_query(&pool, qid, Some("New display name"), CACHED_QUERY)
            .await
            .expect("rename");

        // A first absence must still be just a strike.
        assert_eq!(auto_prune(&pool, qid, &[]).await, 0);
        assert_eq!(remaining_numbers(&pool, qid).await, vec![1]);
        // …and the second one deletes, so the counter is still working.
        assert_eq!(auto_prune(&pool, qid, &[]).await, 1);
    }

    /// Renaming must not reset the fetch timestamps either: the search string is
    /// unchanged, so the cache is exactly as fresh as it was, and a spurious reset
    /// buys nothing but a full re-page of an unchanged result set on the next sync.
    #[tokio::test]
    async fn renaming_a_query_keeps_fetch_timestamps() {
        let (pool, _file) = test_pool().await;
        let id = upsert_query(&pool, "is:pr is:open", "pull_request", None)
            .await
            .expect("upsert");
        mark_fetched(&pool, id, true).await.expect("mark fetched");

        update_query(&pool, id, Some("New display name"), "is:pr is:open")
            .await
            .expect("rename");

        assert!(!is_cache_stale(&pool, id, 300).await.unwrap());
        assert!(!is_full_fetch_due(&pool, id, 300).await.unwrap());
    }

    /// The arming must not outlive the first post-edit fetch: a row the new query does
    /// return is reset by `upsert_items`, so a later transient absence still needs two
    /// strikes.
    #[tokio::test]
    async fn editing_a_query_does_not_arm_rows_the_new_query_returns() {
        let (pool, _file) = test_pool().await;
        let qid = query_with_items(&pool, &[1]).await;

        update_query(&pool, qid, None, "repo:owner/r is:merged")
            .await
            .expect("update");
        // The new query returns #1, disarming it.
        upsert_items(&pool, &[make_item(qid, 1, "PR 1")])
            .await
            .expect("upsert");

        assert_eq!(auto_prune(&pool, qid, &[]).await, 0);
        assert_eq!(remaining_numbers(&pool, qid).await, vec![1]);
    }

    #[tokio::test]
    async fn prune_is_a_noop_when_nothing_is_missing() {
        let (pool, _file) = test_pool().await;
        let qid = query_with_items(&pool, &[1, 2]).await;
        assert_eq!(auto_prune(&pool, qid, &keep(&[1, 2])).await, 0);
        assert_eq!(remaining_numbers(&pool, qid).await, vec![1, 2]);
    }

    /// `idx_items_query_id` was a strict prefix of the index behind
    /// `UNIQUE (query_id, repo_owner, repo_name, number)`, so dropping it removes a
    /// b-tree write per insert and per prune delete without removing a lookup path.
    #[tokio::test]
    async fn query_id_lookups_use_the_unique_index_not_a_dedicated_one() {
        use sqlx::Row;
        let (pool, _file) = test_pool().await;
        let qid = query_with_items(&pool, &[1, 2]).await;

        let indexes: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'items'",
        )
        .fetch_all(&pool)
        .await
        .expect("list indexes");
        assert!(
            !indexes.iter().any(|n| n == "idx_items_query_id"),
            "redundant index still present: {indexes:?}"
        );

        let plan: Vec<String> =
            sqlx::query("EXPLAIN QUERY PLAN SELECT id FROM items WHERE query_id = ?")
                .bind(qid)
                .fetch_all(&pool)
                .await
                .expect("explain")
                .iter()
                .map(|row| row.get::<String, _>("detail"))
                .collect();
        assert!(
            plan.iter()
                .any(|d| d.contains("USING INDEX") || d.contains("USING COVERING INDEX")),
            "query_id lookup fell back to a table scan: {plan:?}"
        );
    }

    /// A failed full walk defers the *retry* without pretending the walk completed:
    /// the completion stamp is what the prune guard compares, so it must not move.
    #[tokio::test]
    async fn mark_full_fetch_attempted_defers_retry_without_claiming_completion() {
        let (pool, _file) = test_pool().await;
        let qid = upsert_query(&pool, "repo:owner/r is:pr", "pull_request", None)
            .await
            .expect("upsert query");

        mark_full_fetch_attempted(&pool, qid).await.expect("mark");

        assert!(
            last_full_fetch_at(&pool, qid)
                .await
                .expect("stamp")
                .is_none(),
            "a failed walk must not stamp the completion column"
        );
        assert!(
            !is_full_fetch_due(&pool, qid, 300).await.expect("due"),
            "the expensive full walk is deferred"
        );
        assert!(
            is_cache_stale(&pool, qid, 300).await.expect("stale"),
            "the incremental retry must still happen promptly"
        );
    }

    /// The prune guard compares completion stamps only, so a *failed* walk of the same
    /// query landing mid-flight no longer costs a successful walk its prune.
    #[tokio::test]
    async fn a_failed_walk_no_longer_blocks_a_concurrent_prune() {
        let (pool, _file) = test_pool().await;
        let qid = query_with_items(&pool, &[1, 2]).await;
        let before_walk = last_full_fetch_at(&pool, qid).await.expect("stamp");

        mark_full_fetch_attempted(&pool, qid).await.expect("mark");

        let deleted = prune_missing_items(&pool, qid, &keep(&[1]), 1, before_walk.as_deref())
            .await
            .expect("prune");
        assert_eq!(deleted, 1);
        assert_eq!(remaining_numbers(&pool, qid).await, vec![1]);
    }

    /// A completed walk stamps both columns, and the attempt column alone decides when
    /// the next full walk is due — a recent failure defers the retry even though the
    /// last *completion* is older than the interval.
    #[tokio::test]
    async fn is_full_fetch_due_reads_the_attempt_stamp() {
        let (pool, _file) = test_pool().await;
        let qid = upsert_query(&pool, "repo:owner/r is:pr", "pull_request", None)
            .await
            .expect("upsert query");

        mark_fetched(&pool, qid, true).await.expect("mark");
        let stamps = sqlx::query!(
            r#"SELECT last_full_fetch_at, last_full_fetch_attempt_at FROM queries WHERE id = ?"#,
            qid,
        )
        .fetch_one(&pool)
        .await
        .expect("stamps");
        assert_eq!(
            stamps.last_full_fetch_at, stamps.last_full_fetch_attempt_at,
            "a completed walk is also an attempt"
        );

        // Age the completion past the interval, leaving a fresh attempt behind.
        sqlx::query!(
            r#"
            UPDATE queries
            SET last_full_fetch_at = datetime('now', '-1 hour')
            WHERE id = ?
            "#,
            qid,
        )
        .execute(&pool)
        .await
        .expect("age completion");
        assert!(
            !is_full_fetch_due(&pool, qid, 300).await.expect("due"),
            "the attempt stamp bounds the retry, not the completion stamp"
        );
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

        let attempt = sqlx::query_scalar!(
            r#"SELECT last_full_fetch_attempt_at FROM queries WHERE id = ?"#,
            id,
        )
        .fetch_one(&pool)
        .await
        .expect("attempt stamp");
        assert!(
            attempt.is_none(),
            "an edited query must not have its full walk deferred by the old attempt"
        );
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

        // #3 survives despite being overflow, because it is unread.
        assert_eq!(remaining_numbers(&pool, qid).await, vec![3, 4, 5]);
    }
}
