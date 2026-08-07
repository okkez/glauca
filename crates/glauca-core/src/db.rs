use anyhow::{Context, Result};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous},
};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::info;

/// Open the cache at `db_path`, creating the file and any missing parent directories
/// and applying pending migrations.
///
/// Fails without touching a SQLite file that glauca did not create — see
/// [`ensure_glauca_cache`], which the caller cannot anticipate from the path alone.
///
/// Every failure names the path: it comes from `--db-path` / `GLAUCA_DB_PATH`, so a bare
/// `Permission denied (os error 13)` would leave the user without the one detail they
/// need to fix it.
pub async fn open_pool(db_path: &Path) -> Result<SqlitePool> {
    // Logged before the work below, so a failure to create or open the path still
    // leaves a record of which path was tried.
    info!(path = %db_path.display(), "opening cache");

    create_parent_dir(db_path)?;
    let pool = SqlitePool::connect_with(connect_options(db_path))
        .await
        .with_context(|| format!("opening cache database {}", db_path.display()))?;
    ensure_glauca_cache(&pool, db_path).await?;

    // A cache from an older or newer glauca gets past that guard and fails here instead.
    // sqlx's error names the offending migration version but not the database file, so
    // attach the path.
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .with_context(|| format!("migrating cache database {}", db_path.display()))?;
    Ok(pool)
}

/// `create_if_missing` creates the cache file but not its directory, and the path can now
/// come from `--db-path` / `GLAUCA_DB_PATH` rather than only the data dir — so make the
/// parent here instead of in each front-end's startup. The empty check matters for a bare
/// relative path like `cache.db`, where `parent()` is `Some("")` and `create_dir_all("")`
/// fails.
fn create_parent_dir(db_path: &Path) -> Result<()> {
    if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating cache directory {}", parent.display()))?;
    }
    Ok(())
}

/// SQLite tuning for this cache: WAL, `synchronous = NORMAL`, and a busy timeout.
fn connect_options(db_path: &Path) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        // WAL + `synchronous = NORMAL`. sqlx deliberately leaves `journal_mode`
        // alone, which leaves SQLite's default rollback journal and
        // `synchronous = FULL`: two to three fsyncs per commit. This cache commits
        // constantly — `upsert_items` commits once per page, per query, per sync
        // cycle — and under a rollback journal a reader blocks on the writer, so the
        // UI's reloads queue behind whichever sync holds the lock (and behind
        // `prune_missing_items`' `BEGIN IMMEDIATE`). Under WAL a commit is an append
        // with the fsync deferred to a checkpoint, and readers never block on the
        // writer — which also matters because a TUI and a GUI can share one cache.
        //
        // What NORMAL gives up: not durability against an application crash (WAL survives
        // that either way, without corruption) but durability against a power loss or
        // kernel panic, which can lose the last few commits. For cached items that cost is
        // cheap — they're re-fetchable from GitHub, so the visible effect is a ghost
        // surviving one extra full-fetch interval. But `cache.db` also holds the user's
        // saved searches (`queries`, `filter_streams`) and the local-only unread markers
        // (`items.last_read_updated_at`, by design never synced anywhere) — none of that is
        // re-fetchable, so a crash within seconds of a save can cost a just-created query or
        // a few read marks. Accepted anyway: the window is seconds and everything in it is
        // cheap for the user to redo. Two sidecar files (`cache.db-wal`, `cache.db-shm`) now
        // live next to the DB.
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        // Block briefly on a locked DB instead of failing immediately — chiefly so
        // a concurrent write during the maintenance pass's VACUUM (which needs
        // exclusive access) waits its turn instead of erroring out its sync cycle.
        .busy_timeout(Duration::from_secs(30))
}

/// Require `db_path` to be a glauca cache, refusing to migrate a SQLite file that belongs
/// to something else.
///
/// Now that the path is user-supplied, `--db-path` can name an existing database by
/// mistake — and migrating one is destructive rather than merely wrong. The initial
/// migration's `CREATE TABLE IF NOT EXISTS` quietly skips a same-named table, while the
/// later `ALTER TABLE ADD COLUMN` migrations still run: aiming glauca at a file holding
/// `queries(foo int)` leaves that table as
/// `queries(foo int, name TEXT, position INTEGER NOT NULL DEFAULT 0, …)` plus the rest of
/// our schema, with no way back. A glauca cache always carries `_sqlx_migrations`, so its
/// absence beside other tables means this database is someone else's.
///
/// FIXME: the protection is scoped to schema and rows, which is what "with no way back"
/// above refers to. The pool has already applied `journal_mode = WAL` by the time this
/// runs and that is a persistent header flag, so a database we refuse can still be left
/// in WAL mode. Inspecting the schema over a connection that sets no pragmas (or before
/// `connect_with`) would remove the side effect.
async fn ensure_glauca_cache(pool: &SqlitePool, db_path: &Path) -> Result<()> {
    let mut tables: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table'")
            .fetch_all(pool)
            .await
            .with_context(|| format!("reading the schema of {}", db_path.display()))?;

    // A brand-new file has no tables at all, which is the normal path.
    let is_fresh_file = tables.is_empty();
    let is_glauca_cache = tables.iter().any(|t| t == "_sqlx_migrations");
    if is_fresh_file || is_glauca_cache {
        return Ok(());
    }

    tables.sort_unstable();
    anyhow::bail!(
        "{} is a SQLite database that glauca did not create (tables: {}) — \
         refusing to migrate it, because that would add glauca's tables and alter \
         the existing ones. Point --db-path/{DB_PATH_ENV} at a new or existing \
         glauca cache instead.",
        db_path.display(),
        tables.join(", "),
    )
}

/// Env var that overrides the cache path at runtime. Deliberately not `DATABASE_URL`:
/// that one is sqlx's compile-time query-verification target (pointed at
/// `crates/glauca-core/dev.db` by `mise.toml` for the whole repo) and has no runtime
/// effect, so reusing it would silently redirect every in-repo `cargo run` to the
/// empty dev schema.
const DB_PATH_ENV: &str = "GLAUCA_DB_PATH";

/// Resolve the cache path: an explicit CLI override wins, then [`DB_PATH_ENV`], then the
/// platform data dir. The single entry point for this — front-ends never build the path
/// themselves, so none of them can bypass the override.
pub fn resolve_db_path(cli_override: Option<PathBuf>) -> PathBuf {
    resolve_db_path_with(cli_override, |k| std::env::var_os(k).map(PathBuf::from))
}

/// Split out of [`resolve_db_path`] so the precedence can be unit-tested without
/// mutating the process environment (which tests share, and run in parallel).
/// Mirrors `github::resolve_token`.
fn resolve_db_path_with(
    cli_override: Option<PathBuf>,
    env: impl Fn(&str) -> Option<PathBuf>,
) -> PathBuf {
    cli_override
        // An empty value reads as "unset" rather than as the empty path, which would
        // otherwise reach SQLite as an unhelpful open error.
        .or_else(|| env(DB_PATH_ENV).filter(|p| !p.as_os_str().is_empty()))
        .unwrap_or_else(default_db_path)
}

fn default_db_path() -> PathBuf {
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
///
/// `id` closes the ordering. `position` alone does not: `created_at` is stored at
/// second granularity, so queries added in the same second tie on both keys, and on a
/// tie SQLite may return the rows in any order — the left pane would then be free to
/// reshuffle itself from one launch to the next.
///
/// Generic over the executor so [`swap_query_positions`] can read this same ordering from
/// *inside* its transaction. That matters beyond convenience: the reorder assigns positions
/// in the order it reads, so if it read through a second, hand-copied `ORDER BY`, the two
/// could drift and the reorder would number an order the left pane never displayed.
pub async fn list_queries<'e, E>(exec: E) -> Result<Vec<QueryRecord>>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let rows = sqlx::query!(
        "SELECT id, query, kind, name FROM queries ORDER BY position ASC, created_at ASC, id ASC"
    )
    .fetch_all(exec)
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
///
/// `id` closes the ordering, and the executor is generic, for the same two reasons they are
/// in [`list_queries`].
pub async fn list_filter_streams<'e, E>(exec: E, parent_id: i64) -> Result<Vec<FilterStreamRecord>>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    // `id!: i64` rather than unwrapping an `Option`: sqlx reads `filter_streams.id` as
    // nullable where it reads `queries.id` as not, and every caller here is reached from one
    // of the engine's spawned tasks, where a panic becomes a dropped `JoinError` — the UI
    // silently not updating, which is the symptom this file exists to stop producing.
    let rows = sqlx::query!(
        r#"SELECT id AS "id!: i64", parent_id, name, filter FROM filter_streams WHERE parent_id = ? ORDER BY position ASC, created_at ASC, id ASC"#,
        parent_id,
    )
    .fetch_all(exec)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| FilterStreamRecord {
            id: r.id,
            parent_id: r.parent_id,
            name: r.name,
            filter: r.filter,
        })
        .collect())
}

/// Insert a new filter stream under a parent query and return its id.
///
/// The new stream lands at the end of its parent's group — see [`upsert_query`] for why
/// the position has to be assigned here rather than left at the column default. Numbering
/// is per parent, matching how [`list_filter_streams`] orders them.
pub async fn upsert_filter_stream(
    pool: &SqlitePool,
    parent_id: i64,
    name: &str,
    filter: &str,
) -> Result<i64> {
    let row = sqlx::query!(
        r#"
        INSERT INTO filter_streams (parent_id, name, filter, position)
        VALUES (
            ?, ?, ?,
            (SELECT COALESCE(MAX(position) + 1, 0) FROM filter_streams WHERE parent_id = ?)
        )
        RETURNING id AS "id!: i64"
        "#,
        parent_id,
        name,
        filter,
        parent_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(row.id)
}

/// Delete a filter stream by id.
pub async fn delete_filter_stream(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query!("DELETE FROM filter_streams WHERE id = ?", id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Exchange the places of two sibling filter streams (they must share the same parent).
///
/// Renumbers that parent's streams for the reasons spelled out on
/// [`swap_query_positions`], including why the transaction is `IMMEDIATE`. Only the parent
/// named by `upper_id` is touched, so the numbering of other queries' streams is left as it
/// is.
///
/// Fails if either stream has been deleted from under the reorder — whichever of the two it
/// is, the call returns that error rather than reporting success. See [`exchanged`] for what a
/// front-end does with a confirmation.
pub async fn swap_filter_stream_positions(
    pool: &SqlitePool,
    upper_id: i64,
    lower_id: i64,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let parent_id = sqlx::query_scalar!(
        "SELECT parent_id FROM filter_streams WHERE id = ?",
        upper_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .with_context(|| format!("filter stream {upper_id} is no longer in the cache"))?;
    let ids: Vec<i64> = list_filter_streams(&mut *tx, parent_id)
        .await?
        .iter()
        .map(|fs| fs.id)
        .collect();
    let ids = exchanged(ids, upper_id, lower_id).with_context(|| {
        format!("filter streams {upper_id} and {lower_id} are no longer both under one query")
    })?;
    for (position, id) in ids.iter().enumerate() {
        let position = position as i64;
        sqlx::query!(
            "UPDATE filter_streams SET position = ? WHERE id = ?",
            position,
            id
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
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

/// Exchange the places of two queries in the left-pane order, so that one moves past the
/// other.
///
/// Written as a renumbering of the whole list rather than as an exchange of the two rows'
/// `position` values, in one transaction. Exchanging the two values is only correct while
/// every position is distinct, and a cache holding queries saved before positions were
/// assigned on insert has them all at the column default: the exchange writes a value back
/// over itself and the reorder is silently lost, in the way [`exchanged`] describes.
/// Renumbering makes the write succeed from any starting state, including one this code cannot
/// produce but an older binary sharing the cache still can, and doing it in a transaction
/// means a crash mid-way cannot leave positions half-assigned.
///
/// The transient duplicates that renumbering writes row by row are also why `position` carries
/// no `UNIQUE` constraint: SQLite checks unique indexes per statement with no way to defer to
/// commit, so the constraint would reject the correct renumbering rather than protect it.
///
/// `BEGIN IMMEDIATE` for the reason spelled out at `prune_missing_items`: this transaction
/// reads before it writes. What is specific to the reorder is what the old shape hid — its
/// autocommit `UPDATE`s did reach the busy handler, so keeping the deferred default here
/// would have made a keypress lose its reorder to whichever sync happened to be mid-commit.
pub async fn swap_query_positions(pool: &SqlitePool, upper_id: i64, lower_id: i64) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let ids: Vec<i64> = list_queries(&mut *tx).await?.iter().map(|q| q.id).collect();
    let ids = exchanged(ids, upper_id, lower_id).with_context(|| {
        format!("queries {upper_id} and {lower_id} are no longer both in the cache")
    })?;
    for (position, id) in ids.iter().enumerate() {
        let position = position as i64;
        sqlx::query!("UPDATE queries SET position = ? WHERE id = ?", position, id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// `ids` with the entries `a` and `b` in each other's place, or `None` if either is absent
/// — another front-end sharing this cache deleted the row after the caller last read the
/// list.
///
/// `None` has to reach the caller as an error rather than as a silent success, and this is the
/// one place that reasoning is written down — the reorder path is built on it from here up to
/// the engine. The engine sends `AppMessage::QueriesSwapped` only when the DB write returns
/// `Ok`, and the TUI and the gpui GUI answer that message by moving the entry in their own
/// `entries` vec without re-reading the DB; the Tauri front-end re-reads instead. So it is
/// those two that a move reported but never recorded would leave showing exactly the bug this
/// renumbering exists to remove: an order that looks applied and is gone on the next launch.
///
/// Adjacency is neither required nor checked: this exchanges the two ids wherever they sit in
/// the list as the DB has it *now*, which is the caller's "move one place" only while nothing
/// has moved between the front-end's last read and this call. `a == b` is a no-op that reports
/// success; `reorder_command` never asks for one.
///
/// FIXME(no cross-process invalidation): another front-end reordering in that gap leaves the
/// two ids non-adjacent, and this then jumps whatever ended up between them while the caller's
/// own `entries` vec moves the entry by one. The old two-value exchange had the same hole, so
/// closing it is a separate job: the front-ends would have to be told to re-read, as the Tauri
/// one already does.
fn exchanged(mut ids: Vec<i64>, a: i64, b: i64) -> Option<Vec<i64>> {
    let a_idx = ids.iter().position(|id| *id == a)?;
    let b_idx = ids.iter().position(|id| *id == b)?;
    ids.swap(a_idx, b_idx);
    Some(ids)
}

/// Upsert a query record and return its id.
///
/// `name` is the optional display name shown in the left pane.
/// If `None` (or empty string), the query string itself is used as the label.
///
/// A newly saved query lands at the end of the left pane, which is what the assigned
/// `position` says and the column's `DEFAULT 0` would not: at 0 the new query would sort
/// among the oldest ones, and where exactly would be left to the `created_at` tie-break.
/// Assigning it here is also what keeps the stored order the authority on what the user
/// sees — a table of identical positions has no order in it to reorder, and would leave
/// [`swap_query_positions`] renumbering rows into whatever sequence the tie-breaks
/// happened to produce.
///
/// The conflicting branch deliberately leaves `position` alone. Re-submitting an existing
/// query string is how a rename reaches the DB, and a rename must not move the query out of
/// the place the user put it with J/K.
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
        INSERT INTO queries (query, kind, name, position)
        VALUES (?, ?, ?, (SELECT COALESCE(MAX(position) + 1, 0) FROM queries))
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

/// How many item keys a prune outcome samples. Bounded because a query edit arms every cached
/// row, so one walk can legitimately find hundreds absent and the only consumer is a log line.
pub const PRUNE_LOG_KEY_CAP: usize = 20;

/// What one prune attempt observed.
///
/// `Skipped` and "nothing was absent" are different facts: a skipped attempt observed nothing
/// at all, so it is not evidence about any row. Returning `0` for both is what used to make
/// the concurrency guard invisible in the logs, and what stopped a test from telling them
/// apart.
#[derive(Debug)]
pub enum PruneOutcome {
    Skipped {
        reason: &'static str,
    },
    Considered {
        /// Rows the query had cached when this walk examined it. The denominator an absence
        /// rate is read against: without it, "0 absences" from a query holding 30 rows and
        /// one holding 1000 look the same.
        cached: usize,
        /// Rows this walk did not return. The full count, not the sample length.
        absent: usize,
        /// Of those, the ones that reached `strikes_required` and were deleted.
        deleted: usize,
        /// Which items were absent, capped at [`PRUNE_LOG_KEY_CAP`]. Identities rather than
        /// rendered strings: how they read is the log's business, and a caller that wants to
        /// act on them shouldn't have to parse `owner/repo#number` back apart.
        absent_keys: Vec<ItemKey>,
        /// Which of those were deleted, under the same cap.
        deleted_keys: Vec<ItemKey>,
    },
}

impl PruneOutcome {
    /// Rows deleted; `0` for a skipped attempt. Only the test helpers that predate this enum
    /// want the count without the rest, so it is not part of the crate's API.
    #[cfg(test)]
    fn deleted(&self) -> usize {
        match self {
            Self::Skipped { .. } => 0,
            Self::Considered { deleted, .. } => *deleted,
        }
    }
}

/// Record that this full fetch didn't return the cached rows absent from `keep`, and
/// delete the ones that have reached `strikes_required` consecutive absences. Returns what the
/// attempt observed — see [`PruneOutcome`], which distinguishes "nothing was absent" from
/// "the guard below skipped this walk entirely".
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
) -> Result<PruneOutcome> {
    use std::collections::HashSet;
    let keep_set: HashSet<(&str, &str, i64)> = keep
        .iter()
        .map(|(owner, name, number)| (owner.as_str(), name.as_str(), *number))
        .collect();

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
    //
    // The guard runs before the "nothing was absent" exit, not after: a stale walk that
    // happens to find everything present still observed nothing independent, and reporting
    // that as `absent = 0` would feed a non-observation into the denominator the
    // transient-absence measurement is read against.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if last_full_fetch_at(&mut *tx, query_id).await?.as_deref() != last_full_fetch_before_walk {
        tx.rollback().await?;
        return Ok(PruneOutcome::Skipped {
            reason: "a concurrent full fetch finished first",
        });
    }

    // Read the cached rows inside the transaction, not before it: another front-end's
    // `upsert_items` landing in that gap would otherwise let this walk strike a row the cache
    // had just re-acquired — the same "not an independent observation" the guard above rules
    // out, one step earlier.
    let cached_rows = sqlx::query!(
        r#"SELECT id AS "id!: i64", repo_owner, repo_name, number FROM items WHERE query_id = ?"#,
        query_id,
    )
    .fetch_all(&mut *tx)
    .await?;
    let cached = cached_rows.len();

    // Whole rows, not just ids: the log line names the absent items, and re-querying for
    // their keys after the delete would be too late.
    let missing: Vec<_> = cached_rows
        .into_iter()
        .filter(|r| !keep_set.contains(&(r.repo_owner.as_str(), r.repo_name.as_str(), r.number)))
        .collect();
    let absent = missing.len();
    let absent_keys: Vec<ItemKey> = missing
        .iter()
        .take(PRUNE_LOG_KEY_CAP)
        .map(|r| (r.repo_owner.clone(), r.repo_name.clone(), r.number))
        .collect();

    if missing.is_empty() {
        tx.rollback().await?;
        return Ok(PruneOutcome::Considered {
            cached,
            absent,
            deleted: 0,
            absent_keys,
            deleted_keys: Vec::new(),
        });
    }

    let missing_ids: Vec<i64> = missing.iter().map(|r| r.id).collect();

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
    // `RETURNING` rather than `rows_affected()` plus a Rust-side re-derivation of which rows
    // met the threshold: the deleted keys then come from the statement that did the deleting,
    // so the log can't disagree with the database about what went.
    let deleted_rows = sqlx::query!(
        r#"
        DELETE FROM items
        WHERE missing_count >= ? AND id IN (SELECT value FROM json_each(?))
        RETURNING repo_owner, repo_name, number AS "number!: i64"
        "#,
        strikes_required,
        ids,
    )
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(PruneOutcome::Considered {
        cached,
        absent,
        deleted: deleted_rows.len(),
        absent_keys,
        deleted_keys: deleted_rows
            .into_iter()
            .take(PRUNE_LOG_KEY_CAP)
            .map(|r| (r.repo_owner, r.repo_name, r.number))
            .collect(),
    })
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

/// When `query_id` last *completed* a full fetch, or `None` if never.
///
/// Reads the completion stamp that the prune concurrency guard compares against: generic
/// over the executor because [`prune_missing_items`] must re-read it *inside* its
/// transaction for that comparison to mean anything, while the only other production
/// caller — `sync_task`'s pre-walk snapshot (`engine.rs`) — reads it from the pool.
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

/// Stamp `last_full_fetch_attempt_at` without touching `last_fetched_at` or the
/// completion stamp: a full walk was attempted and failed.
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
    use crate::test_support::{foreign_database, make_item, raw_pool, test_pool};

    /// The three front-ends resolve their cache path through one function, so the
    /// precedence is pinned here rather than left to whichever one is read first.
    /// Driving `resolve_db_path_with` directly keeps this off the process environment,
    /// which every other test in this binary shares.
    #[test]
    fn resolve_db_path_prefers_cli_then_env_then_default() {
        let env = |_: &str| Some(PathBuf::from("/env/cache.db"));

        assert_eq!(
            resolve_db_path_with(Some(PathBuf::from("/cli/cache.db")), env),
            PathBuf::from("/cli/cache.db"),
        );
        assert_eq!(
            resolve_db_path_with(None, env),
            PathBuf::from("/env/cache.db"),
        );
        assert_eq!(resolve_db_path_with(None, |_| None), default_db_path());
    }

    /// `GLAUCA_DB_PATH=` (exported but empty) is the shape a shell profile or CI job
    /// produces by accident. Treating it as the empty path would hand SQLite something
    /// it can only fail to open, so it has to read as unset.
    #[test]
    fn resolve_db_path_treats_an_empty_env_value_as_unset() {
        assert_eq!(
            resolve_db_path_with(None, |_| Some(PathBuf::new())),
            default_db_path(),
        );
    }

    /// `create_if_missing` creates the file but not its directory — the assumption
    /// behind `open_pool` doing the `create_dir_all` itself. A user-supplied
    /// `--db-path` can name a directory that does not exist yet.
    #[tokio::test]
    async fn open_pool_creates_a_missing_parent_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("nested").join("deeper").join("cache.db");

        let pool = open_pool(&db_path)
            .await
            .unwrap_or_else(|e| panic!("open pool: {e:#}"));

        assert!(db_path.exists(), "database file was not created");
        // Migrated, not merely touched: the front-ends expect a usable cache, so the
        // query itself is the assertion.
        sqlx::query("SELECT id FROM queries LIMIT 1")
            .fetch_optional(&pool)
            .await
            .expect("a migrated cache has a queries table");
    }

    /// A `--db-path` naming an unwritable or nonsensical location surfaces as an
    /// `io::Error` with no path in it (`Permission denied (os error 13)` and nothing
    /// else), which says nothing about what to fix. Pin the path into the message.
    /// Uses a file as the parent because that fails the same way everywhere, unlike a
    /// permissions test.
    #[tokio::test]
    async fn open_pool_names_the_directory_it_could_not_create() {
        let blocker = tempfile::NamedTempFile::new().expect("tempfile");
        let missing_parent = blocker.path().join("subdir");
        let db_path = missing_parent.join("cache.db");

        let err = open_pool(&db_path)
            .await
            .expect_err("a file cannot be a parent directory");

        let msg = format!("{err:#}");
        assert!(
            msg.contains(&missing_parent.display().to_string()),
            "error should name the directory it failed to create, got: {msg}"
        );
    }

    /// The table a foreign database and glauca both want to own, which is what makes
    /// migrating one destructive. Shared by the two tests below so the "unchanged" claim
    /// is compared against the very string that created it.
    const FOREIGN_SCHEMA: &str = "CREATE TABLE queries (foo INTEGER)";

    /// Aiming `--db-path` at another application's database has to say so, and say which
    /// file and what it found there.
    #[tokio::test]
    async fn open_pool_refuses_a_database_glauca_did_not_create() {
        let file = foreign_database(FOREIGN_SCHEMA).await;

        let err = open_pool(file.path())
            .await
            .expect_err("a foreign database must not be migrated");

        let msg = format!("{err:#}");
        assert!(
            msg.contains(&file.path().display().to_string()) && msg.contains("queries"),
            "error should name the database and what it found, got: {msg}"
        );
    }

    /// Refusing used to come too late: `CREATE TABLE IF NOT EXISTS` skipped the same-named
    /// table but the `ALTER TABLE` migrations still ran, so the user's `queries` table came
    /// back with glauca's columns grafted on. Nothing may be added or altered.
    #[tokio::test]
    async fn open_pool_leaves_a_refused_database_untouched() {
        let file = foreign_database(FOREIGN_SCHEMA).await;

        open_pool(file.path())
            .await
            .expect_err("a foreign database must not be migrated");

        let pool = raw_pool(file.path()).await;
        let schema: Vec<(String, String)> =
            sqlx::query_as("SELECT name, sql FROM sqlite_master WHERE type = 'table'")
                .fetch_all(&pool)
                .await
                .expect("read the schema back");
        assert_eq!(
            schema,
            vec![("queries".to_string(), FOREIGN_SCHEMA.to_string())]
        );
    }

    /// Opening a cache written by a newer glauca — or an older binary reading one it
    /// cannot understand — must not be mistaken for a fresh file. sqlx reports the
    /// offending migration version but not the file, so the path has to come from us.
    #[tokio::test]
    async fn open_pool_names_the_database_it_could_not_migrate() {
        let (pool, file) = test_pool().await;
        record_a_migration_from_the_future(&pool, 99_999_999).await;
        pool.close().await;

        let err = open_pool(file.path())
            .await
            .expect_err("an unknown applied migration must not be ignored");

        let msg = format!("{err:#}");
        assert!(
            msg.contains(&file.path().display().to_string()),
            "error should name the database it failed to migrate, got: {msg}"
        );
    }

    /// Mark `version` as already applied even though this build has no such migration —
    /// what a rollback to an older binary looks like from sqlx's side. The other columns
    /// are `NOT NULL` filler that no assertion reads.
    async fn record_a_migration_from_the_future(pool: &SqlitePool, version: i64) {
        sqlx::query(
            "INSERT INTO _sqlx_migrations
             (version, description, installed_on, success, checksum, execution_time)
             VALUES (?, 'from the future', CURRENT_TIMESTAMP, 1, x'00', 0)",
        )
        .bind(version)
        .execute(pool)
        .await
        .expect("record a migration this build does not have");
    }

    /// The cache commits per page, per query, per sync cycle, so the journal mode is a
    /// performance decision worth pinning down rather than inheriting from sqlx's
    /// defaults. `PRAGMA synchronous` answers with an integer: 1 is NORMAL.
    #[tokio::test]
    async fn open_pool_uses_wal_with_normal_synchronous() {
        let (pool, _file) = test_pool().await;

        let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await
            .expect("journal_mode");
        assert_eq!(journal_mode, "wal");

        let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(&pool)
            .await
            .expect("synchronous");
        assert_eq!(synchronous, 1);
    }

    /// `vacuum` is the one place the cache takes an exclusive lock, and the journal
    /// mode changes how that lock is taken — so drive both of its branches against a
    /// real WAL database instead of trusting that `VACUUM` and WAL compose.
    #[tokio::test]
    async fn vacuum_runs_only_once_enough_pages_are_free() {
        let (pool, _file) = test_pool().await;
        let qid = upsert_query(&pool, "repo:owner/r is:pr", "pull_request", None)
            .await
            .expect("upsert query");

        // A fresh cache has nothing to reclaim, so the sweep must skip the rewrite.
        assert!(!vacuum(&pool).await.expect("vacuum on a fresh cache"));

        // Push past `VACUUM_MIN_FREELIST_PAGES` (256 pages ≈ 1 MiB) with re-fetchable
        // bodies, then drop the query so those pages land on the freelist.
        let body = "x".repeat(8 * 1024);
        for number in 1..=400 {
            let mut item = make_item(qid, number, "Filler");
            item.body = Some(body.clone());
            upsert_item(&pool, &item).await.expect("upsert");
        }
        delete_query(&pool, qid).await.expect("delete query");

        assert!(vacuum(&pool).await.expect("vacuum with a full freelist"));

        // The full-file rewrite must not have knocked the database out of WAL.
        let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await
            .expect("journal_mode");
        assert_eq!(journal_mode, "wal");
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

    /// Insertion order, not `created_at` order: since positions are assigned on insert it is the
    /// `position` key that decides this. The other two keys have their own tests —
    /// `list_queries_breaks_ties_by_id` and `list_queries_falls_back_to_created_at`.
    #[tokio::test]
    async fn list_queries_returns_in_insertion_order() {
        let (pool, _file) = test_pool().await;

        let [first, second, third] = seed_three_queries(&pool).await;

        assert_eq!(query_ids_in_order(&pool).await, vec![first, second, third]);
    }

    /// The seed the query reorder tests (and the insertion-order test above) start from: three
    /// queries in a known order, so each body opens with the state it is actually about.
    /// [`seed_parent_with_three_streams`] is the filter-stream counterpart.
    async fn seed_three_queries(pool: &SqlitePool) -> [i64; 3] {
        [
            seed_query(pool, "query:first").await,
            seed_query(pool, "query:second").await,
            seed_query(pool, "query:third").await,
        ]
    }

    async fn seed_query(pool: &SqlitePool, query: &str) -> i64 {
        upsert_query(pool, query, "issue", None)
            .await
            .expect("seed query")
    }

    /// Three streams under one parent, returning the parent alongside them.
    async fn seed_parent_with_three_streams(pool: &SqlitePool) -> (i64, [i64; 3]) {
        let parent = seed_query(pool, "query:parent").await;
        let streams = [
            seed_filter_stream(pool, parent, "first", "state:open").await,
            seed_filter_stream(pool, parent, "second", "state:closed").await,
            seed_filter_stream(pool, parent, "third", "is:draft").await,
        ];
        (parent, streams)
    }

    async fn seed_filter_stream(pool: &SqlitePool, parent: i64, name: &str, filter: &str) -> i64 {
        upsert_filter_stream(pool, parent, name, filter)
            .await
            .expect("seed filter stream")
    }

    /// `position` is an implementation detail of the left-pane order, so the tests
    /// that need to see it read the column instead of going through a public fn.
    async fn query_position(pool: &SqlitePool, id: i64) -> i64 {
        sqlx::query_scalar!("SELECT position FROM queries WHERE id = ?", id)
            .fetch_one(pool)
            .await
            .expect("read query position")
    }

    async fn filter_stream_position(pool: &SqlitePool, id: i64) -> i64 {
        sqlx::query_scalar!("SELECT position FROM filter_streams WHERE id = ?", id)
            .fetch_one(pool)
            .await
            .expect("read filter stream position")
    }

    /// The positions of `ids`, in the order given — so an assertion can pair a display order
    /// with the numbering behind it in one line.
    async fn query_positions(pool: &SqlitePool, ids: &[i64]) -> Vec<i64> {
        let mut positions = Vec::with_capacity(ids.len());
        for id in ids {
            positions.push(query_position(pool, *id).await);
        }
        positions
    }

    async fn filter_stream_positions(pool: &SqlitePool, ids: &[i64]) -> Vec<i64> {
        let mut positions = Vec::with_capacity(ids.len());
        for id in ids {
            positions.push(filter_stream_position(pool, *id).await);
        }
        positions
    }

    /// The ids in left-pane order — named for ids rather than "order" so it cannot be misread as
    /// the sibling `query_positions`, which returns `position` values and the same `Vec<i64>`.
    async fn query_ids_in_order(pool: &SqlitePool) -> Vec<i64> {
        list_queries(pool)
            .await
            .expect("list queries")
            .iter()
            .map(|q| q.id)
            .collect()
    }

    async fn filter_stream_ids_in_order(pool: &SqlitePool, parent_id: i64) -> Vec<i64> {
        list_filter_streams(pool, parent_id)
            .await
            .expect("list filter streams")
            .iter()
            .map(|fs| fs.id)
            .collect()
    }

    /// A query at position 0 with a fixed `created_at` — tied on both sort keys with every other
    /// row this helper inserts, so only `id` is left to decide their order. The id is explicit
    /// because that is what the assertion is about.
    async fn insert_tied_query(pool: &SqlitePool, id: i64, query: &str) {
        sqlx::query!(
            "INSERT INTO queries (id, query, kind, position, created_at)
             VALUES (?, ?, 'issue', 0, '2026-08-07 00:00:00')",
            id,
            query,
        )
        .execute(pool)
        .await
        .expect("insert tied query");
    }

    async fn insert_tied_filter_stream(pool: &SqlitePool, id: i64, parent: i64, name: &str) {
        sqlx::query!(
            "INSERT INTO filter_streams (id, parent_id, name, filter, position, created_at)
             VALUES (?, ?, ?, 'state:open', 0, '2026-08-07 00:00:00')",
            id,
            parent,
            name,
        )
        .execute(pool)
        .await
        .expect("insert tied filter stream");
    }

    /// Ties on both `position` and `created_at` are reachable in ordinary use — several queries
    /// added inside one second, or a cache written before positions were assigned on insert — so
    /// which row wins is pinned here instead of being left to SQLite, which is free to answer
    /// differently on each launch.
    #[tokio::test]
    async fn list_queries_breaks_ties_by_id() {
        let (pool, _file) = test_pool().await;

        // Inserted out of id order, so the answer cannot come from the insertion order.
        insert_tied_query(&pool, 7, "query:c").await;
        insert_tied_query(&pool, 5, "query:a").await;
        insert_tied_query(&pool, 6, "query:b").await;

        assert_eq!(query_ids_in_order(&pool).await, vec![5, 6, 7]);
    }

    #[tokio::test]
    async fn list_filter_streams_breaks_ties_by_id() {
        let (pool, _file) = test_pool().await;
        let parent = seed_query(&pool, "query:parent").await;

        insert_tied_filter_stream(&pool, 7, parent, "c").await;
        insert_tied_filter_stream(&pool, 5, parent, "a").await;
        insert_tied_filter_stream(&pool, 6, parent, "b").await;

        assert_eq!(
            filter_stream_ids_in_order(&pool, parent).await,
            vec![5, 6, 7]
        );
    }

    /// The middle sort key, which the two tests above leave untested: on a cache the backfill
    /// migration has not reached yet, every row sits at position 0 and `created_at` is what
    /// actually orders the left pane.
    #[tokio::test]
    async fn list_queries_falls_back_to_created_at() {
        let (pool, _file) = test_pool().await;

        // Ids ascend as the timestamps descend, so id order and created_at order disagree.
        for (id, query, created_at) in [
            (5_i64, "query:a", "2026-08-07 00:00:03"),
            (6, "query:b", "2026-08-07 00:00:02"),
            (7, "query:c", "2026-08-07 00:00:01"),
        ] {
            sqlx::query!(
                "INSERT INTO queries (id, query, kind, position, created_at)
                 VALUES (?, ?, 'issue', 0, ?)",
                id,
                query,
                created_at,
            )
            .execute(&pool)
            .await
            .expect("insert");
        }

        assert_eq!(query_ids_in_order(&pool).await, vec![7, 6, 5]);
    }

    /// `include_str!` rather than a copy, so a rewrite of the migration is what this runs.
    const BACKFILL_POSITIONS_MIGRATION: &str =
        include_str!("../migrations/20260807000001_backfill_positions.sql");

    /// Every other test applies this migration to empty tables, where it ranks nothing — so
    /// running it proves only that it parses. The one way it can silently be wrong is the
    /// reason it is written as a window function feeding `UPDATE ... FROM` instead of the
    /// correlated `COUNT(*)` the earlier position migrations used: it ranks rows by the very
    /// column it is rewriting, and reading that column mid-update would make the result
    /// depend on the order rows happen to be visited. So give it rows whose positions
    /// disagree with their ids and pin both halves of what it promises — dense numbering,
    /// and an order the user cannot see change.
    #[tokio::test]
    async fn backfill_positions_migration_densifies_without_moving_anything() {
        let (pool, _file) = test_pool().await;

        // Positions that disagree with the ids, so a ranking that read `position` mid-update
        // could not land on the same answer by accident.
        for (id, query, position) in [
            (1_i64, "query:a", 30_i64),
            (2, "query:b", 10),
            (3, "query:c", 20),
        ] {
            sqlx::query!(
                "INSERT INTO queries (id, query, kind, position) VALUES (?, ?, 'issue', ?)",
                id,
                query,
                position,
            )
            .execute(&pool)
            .await
            .expect("seed query");
        }
        // Two parents, so per-parent numbering is exercised, with the second parent's pair tied
        // on `position` so the id tie-break is carried into the new numbering too.
        // Stream ids start at 11 so that an expected list of stream ids cannot be misread as a
        // parent id: parents are 1 and 2 here.
        for (id, parent_id, name, position) in [
            (11_i64, 1_i64, "a", 7_i64),
            (12, 1, "b", 3),
            (13, 2, "c", 9),
            (14, 2, "d", 9),
        ] {
            sqlx::query!(
                "INSERT INTO filter_streams (id, parent_id, name, filter, position)
                 VALUES (?, ?, ?, 'state:open', ?)",
                id,
                parent_id,
                name,
                position,
            )
            .execute(&pool)
            .await
            .expect("seed filter stream");
        }

        // The order the left pane displays before the migration: by position, and within the
        // parent-2 tie at position 9, by id.
        assert_eq!(query_ids_in_order(&pool).await, vec![2, 3, 1]);
        assert_eq!(filter_stream_ids_in_order(&pool, 1).await, vec![12, 11]);
        assert_eq!(filter_stream_ids_in_order(&pool, 2).await, vec![13, 14]);

        sqlx::raw_sql(BACKFILL_POSITIONS_MIGRATION)
            .execute(&pool)
            .await
            .expect("run the backfill migration");

        // The same order, now recorded as 0..n-1 — per parent for the streams, rather than
        // continuing across the table.
        assert_eq!(query_ids_in_order(&pool).await, vec![2, 3, 1]);
        assert_eq!(query_positions(&pool, &[2, 3, 1]).await, vec![0, 1, 2]);
        assert_eq!(filter_stream_ids_in_order(&pool, 1).await, vec![12, 11]);
        assert_eq!(filter_stream_positions(&pool, &[12, 11]).await, vec![0, 1]);
        assert_eq!(filter_stream_ids_in_order(&pool, 2).await, vec![13, 14]);
        assert_eq!(filter_stream_positions(&pool, &[13, 14]).await, vec![0, 1]);
    }

    /// Pins the first half of `upsert_query`'s position handling — see its doc for why a new
    /// query must not be left at the column default.
    #[tokio::test]
    async fn upsert_query_appends_after_the_existing_queries() {
        let (pool, _file) = test_pool().await;

        let first = seed_query(&pool, "query:first").await;
        let second = seed_query(&pool, "query:second").await;

        assert_eq!(query_position(&pool, first).await, 0);
        assert_eq!(query_position(&pool, second).await, 1);
    }

    /// The other half: the conflicting branch, which is how a rename reaches the DB, must leave
    /// the row where the user put it.
    #[tokio::test]
    async fn upsert_query_keeps_the_position_when_only_the_name_changes() {
        let (pool, _file) = test_pool().await;
        let first = seed_query(&pool, "query:first").await;
        seed_query(&pool, "query:second").await;

        let again = upsert_query(&pool, "query:first", "issue", Some("First"))
            .await
            .expect("upsert");

        assert_eq!(again, first);
        assert_eq!(query_position(&pool, first).await, 0);
    }

    /// The front-ends move the entry in their own `entries` vec as soon as the engine confirms
    /// the swap, so an in-session reorder looks right whether or not the DB changed. What has to
    /// be asserted is the part the user only sees on the next launch: that the order came from
    /// the DB.
    #[tokio::test]
    async fn swap_query_positions_persists_the_new_order() {
        let (pool, _file) = test_pool().await;
        let [first, second, third] = seed_three_queries(&pool).await;

        swap_query_positions(&pool, first, second)
            .await
            .expect("swap");

        assert_eq!(query_ids_in_order(&pool).await, vec![second, first, third]);
    }

    /// Every query saved before positions were assigned on insert sits at the schema default,
    /// and an older binary sharing this cache still writes them that way. Reordering has to work
    /// from a table of duplicates rather than only from one the migration has already tidied.
    #[tokio::test]
    async fn swap_query_positions_persists_the_new_order_when_positions_are_all_equal() {
        let (pool, _file) = test_pool().await;
        let [first, second, third] = seed_three_queries(&pool).await;
        sqlx::query!("UPDATE queries SET position = 0")
            .execute(&pool)
            .await
            .expect("flatten positions");

        swap_query_positions(&pool, second, third)
            .await
            .expect("swap");

        assert_eq!(query_ids_in_order(&pool).await, vec![first, third, second]);
    }

    /// Front-ends sharing one cache can be showing a query another of them has deleted, so a
    /// reorder can name a row that is no longer there. See `exchanged` for why that has to fail
    /// rather than report a move the DB never made.
    #[tokio::test]
    async fn swap_query_positions_fails_when_a_query_is_gone() {
        let (pool, _file) = test_pool().await;
        let [first, second, third] = seed_three_queries(&pool).await;
        // Spread the survivors' positions out, so that "nothing was written" is
        // distinguishable from "the survivors were renumbered": with dense positions a
        // rolled-back transaction and a completed renumbering leave the same two values.
        sqlx::query!("UPDATE queries SET position = 5 WHERE id = ?", first)
            .execute(&pool)
            .await
            .expect("spread positions");
        sqlx::query!("UPDATE queries SET position = 9 WHERE id = ?", third)
            .execute(&pool)
            .await
            .expect("spread positions");
        delete_query(&pool, second).await.expect("delete");

        assert!(swap_query_positions(&pool, first, second).await.is_err());
        assert!(swap_query_positions(&pool, second, first).await.is_err());
        assert_eq!(query_position(&pool, first).await, 5);
        assert_eq!(query_position(&pool, third).await, 9);
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

    /// Filter streams are ordered within their parent query, so their positions are
    /// numbered per parent: a second query's first stream starts over at 0 instead of
    /// continuing the first query's numbering.
    #[tokio::test]
    async fn upsert_filter_stream_appends_within_its_own_parent() {
        let (pool, _file) = test_pool().await;

        let parent_a = upsert_query(&pool, "query:a", "issue", None)
            .await
            .expect("upsert query");
        let parent_b = upsert_query(&pool, "query:b", "issue", None)
            .await
            .expect("upsert query");

        let a_first = upsert_filter_stream(&pool, parent_a, "A first", "state:open")
            .await
            .expect("upsert filter stream");
        let a_second = upsert_filter_stream(&pool, parent_a, "A second", "state:closed")
            .await
            .expect("upsert filter stream");
        let b_first = upsert_filter_stream(&pool, parent_b, "B first", "state:open")
            .await
            .expect("upsert filter stream");

        assert_eq!(filter_stream_position(&pool, a_first).await, 0);
        assert_eq!(filter_stream_position(&pool, a_second).await, 1);
        assert_eq!(filter_stream_position(&pool, b_first).await, 0);
    }

    #[tokio::test]
    async fn swap_filter_stream_positions_persists_the_new_order() {
        let (pool, _file) = test_pool().await;
        let (parent, [first, second, third]) = seed_parent_with_three_streams(&pool).await;

        swap_filter_stream_positions(&pool, first, second)
            .await
            .expect("swap");

        assert_eq!(
            filter_stream_ids_in_order(&pool, parent).await,
            vec![second, first, third]
        );
    }

    /// The sibling counterpart of
    /// `swap_query_positions_persists_the_new_order_when_positions_are_all_equal`: streams saved
    /// before positions were assigned on insert are all at the default.
    #[tokio::test]
    async fn swap_filter_stream_positions_persists_the_new_order_when_positions_are_all_equal() {
        let (pool, _file) = test_pool().await;
        let (parent, [first, second, third]) = seed_parent_with_three_streams(&pool).await;
        sqlx::query!("UPDATE filter_streams SET position = 0")
            .execute(&pool)
            .await
            .expect("flatten positions");

        swap_filter_stream_positions(&pool, second, third)
            .await
            .expect("swap");

        assert_eq!(
            filter_stream_ids_in_order(&pool, parent).await,
            vec![first, third, second]
        );
    }

    /// Streams are numbered within their parent, so a reorder renumbers one group and has to
    /// leave every other group's numbering as it found it.
    #[tokio::test]
    async fn swap_filter_stream_positions_leaves_other_parents_alone() {
        let (pool, _file) = test_pool().await;
        let (_, [first, second, _]) = seed_parent_with_three_streams(&pool).await;
        let other_parent = seed_query(&pool, "query:other").await;
        let untouched = seed_filter_stream(&pool, other_parent, "elsewhere", "state:open").await;

        swap_filter_stream_positions(&pool, first, second)
            .await
            .expect("swap");

        assert_eq!(filter_stream_position(&pool, untouched).await, 0);
    }

    /// The sibling counterpart of `swap_query_positions_fails_when_a_query_is_gone`. Both
    /// arguments are covered because the two ids reach the DB by different routes here: the
    /// upper one through the parent lookup, the lower one through `exchanged`.
    #[tokio::test]
    async fn swap_filter_stream_positions_fails_when_a_stream_is_gone() {
        let (pool, _file) = test_pool().await;
        let (_, [first, second, third]) = seed_parent_with_three_streams(&pool).await;
        // Sparse for the reason given in the query counterpart above.
        sqlx::query!("UPDATE filter_streams SET position = 5 WHERE id = ?", first)
            .execute(&pool)
            .await
            .expect("spread positions");
        sqlx::query!("UPDATE filter_streams SET position = 9 WHERE id = ?", third)
            .execute(&pool)
            .await
            .expect("spread positions");
        delete_filter_stream(&pool, second).await.expect("delete");

        assert!(
            swap_filter_stream_positions(&pool, first, second)
                .await
                .is_err()
        );
        assert!(
            swap_filter_stream_positions(&pool, second, first)
                .await
                .is_err()
        );
        assert_eq!(filter_stream_position(&pool, first).await, 5);
        assert_eq!(filter_stream_position(&pool, third).await, 9);
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

    /// The cache keys for `numbers`, as `query_with_items` stores them.
    fn item_keys(numbers: &[i64]) -> Vec<ItemKey> {
        numbers
            .iter()
            .map(|n| ("owner".to_string(), "repo".to_string(), *n))
            .collect()
    }

    /// What a search returned — i.e. the rows a prune must leave alone. Same values as
    /// [`item_keys`]; the two names keep an argument ("these came back") from reading like an
    /// expectation ("these went missing").
    fn keep(numbers: &[i64]) -> Vec<ItemKey> {
        item_keys(numbers)
    }

    /// The next search returned `numbers`, which clears their strikes.
    async fn search_returned(pool: &SqlitePool, query_id: i64, numbers: &[i64]) {
        let items: Vec<CachedItem> = numbers
            .iter()
            .map(|n| make_item(query_id, *n, &format!("PR {n}")))
            .collect();
        upsert_items(pool, &items).await.expect("upsert");
    }

    /// The one place these tests call `prune_missing_items`, so its argument list is written
    /// once. The helpers around it name the shapes the production callers actually take.
    async fn prune_outcome(
        pool: &SqlitePool,
        query_id: i64,
        keep: &[ItemKey],
        strikes_required: i64,
        last_full_fetch_before_walk: Option<&str>,
    ) -> PruneOutcome {
        prune_missing_items(
            pool,
            query_id,
            keep,
            strikes_required,
            last_full_fetch_before_walk,
        )
        .await
        .expect("prune")
    }

    /// Prune as a fetch that observed `last_full_fetch_before_walk` before its walk would.
    async fn prune_observing(
        pool: &SqlitePool,
        query_id: i64,
        keep: &[ItemKey],
        last_full_fetch_before_walk: Option<&str>,
    ) -> usize {
        prune_outcome(
            pool,
            query_id,
            keep,
            PRUNE_STRIKES,
            last_full_fetch_before_walk,
        )
        .await
        .deleted()
    }

    /// An automatic sync's prune of a query that has never been full fetched (so the
    /// stamp it observed is `None`), as [`PruneOutcome`] rather than a count — for tests that
    /// need to tell a skip from an attempt that found nothing absent.
    async fn auto_prune_outcome(
        pool: &SqlitePool,
        query_id: i64,
        keep: &[ItemKey],
    ) -> PruneOutcome {
        prune_outcome(pool, query_id, keep, PRUNE_STRIKES, None).await
    }

    async fn auto_prune(pool: &SqlitePool, query_id: i64, keep: &[ItemKey]) -> usize {
        auto_prune_outcome(pool, query_id, keep).await.deleted()
    }

    /// A user's explicit resync: `PruneTrust::Immediate`, i.e. delete on first absence.
    async fn forced_prune_outcome(
        pool: &SqlitePool,
        query_id: i64,
        keep: &[ItemKey],
    ) -> PruneOutcome {
        prune_outcome(pool, query_id, keep, 1, None).await
    }

    async fn forced_prune(pool: &SqlitePool, query_id: i64, keep: &[ItemKey]) -> usize {
        forced_prune_outcome(pool, query_id, keep).await.deleted()
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

    /// A guarded prune observed nothing at all, which is a different fact from "nothing was
    /// absent". Collapsing both into `0` is what makes the guard invisible in the logs.
    #[tokio::test]
    async fn prune_reports_a_skip_rather_than_zero_deletions() {
        let (pool, _file) = test_pool().await;
        let qid = query_with_items(&pool, &[1, 2]).await;

        // Walk A observed no stamp, pruned (strike one), and marked the query fetched.
        assert_eq!(auto_prune(&pool, qid, &keep(&[1])).await, 0);
        mark_fetched(&pool, qid, true).await.expect("mark");

        // Walk B started before that and also observed no stamp.
        let outcome = auto_prune_outcome(&pool, qid, &keep(&[1])).await;
        assert!(
            matches!(outcome, PruneOutcome::Skipped { .. }),
            "a guarded prune must be distinguishable from one that found nothing absent, \
             got {outcome:?}"
        );
    }

    /// The guard runs even when nothing was absent. A stale walk observed nothing
    /// independent, so reporting it as `absent=0` would put a non-observation into the
    /// denominator the transient-absence measurement is read against.
    #[tokio::test]
    async fn prune_reports_a_skip_even_when_nothing_was_absent() {
        let (pool, _file) = test_pool().await;
        let qid = query_with_items(&pool, &[1]).await;

        // A concurrent walk completed and stamped the query after this one started.
        mark_fetched(&pool, qid, true).await.expect("mark");

        // This walk returned everything it had cached, so nothing is absent — but it still
        // observed a stamp that has since moved.
        let outcome = auto_prune_outcome(&pool, qid, &keep(&[1])).await;
        assert!(
            matches!(outcome, PruneOutcome::Skipped { .. }),
            "a stale walk must report a skip whether or not it found rows absent, got {outcome:?}"
        );
    }

    /// What the log line reports: the absent keys on strike one, and the deleted keys on the
    /// strike that removes them.
    #[tokio::test]
    async fn prune_reports_absent_then_deleted_keys() {
        let (pool, _file) = test_pool().await;
        let qid = query_with_items(&pool, &[1, 2]).await;

        let PruneOutcome::Considered {
            absent,
            deleted,
            absent_keys,
            deleted_keys,
            ..
        } = auto_prune_outcome(&pool, qid, &keep(&[1])).await
        else {
            panic!("the fixture has one row absent, so this walk must have considered it");
        };
        assert_eq!(
            (absent, deleted),
            (1, 0),
            "strike one records, deletes nothing"
        );
        assert_eq!(absent_keys, item_keys(&[2]));
        assert!(deleted_keys.is_empty());

        let PruneOutcome::Considered {
            absent,
            deleted,
            absent_keys,
            deleted_keys,
            ..
        } = auto_prune_outcome(&pool, qid, &keep(&[1])).await
        else {
            panic!("the second walk must have considered the same row");
        };
        assert_eq!((absent, deleted), (1, 1), "strike two deletes");
        assert_eq!(absent_keys, item_keys(&[2]));
        assert_eq!(deleted_keys, item_keys(&[2]));
        assert_eq!(remaining_numbers(&pool, qid).await, vec![1]);
    }

    /// `cached` is the row count the walk examined, so an absence count can be read as a rate
    /// rather than a bare number. It counts what was there when the walk started, including
    /// the rows the same walk goes on to delete.
    #[tokio::test]
    async fn prune_reports_how_many_rows_it_examined() {
        let (pool, _file) = test_pool().await;
        let qid = query_with_items(&pool, &[1, 2, 3]).await;

        let PruneOutcome::Considered { cached, absent, .. } =
            auto_prune_outcome(&pool, qid, &keep(&[1, 2, 3])).await
        else {
            panic!("nothing stamped the query, so the guard cannot have skipped this walk");
        };
        assert_eq!((cached, absent), (3, 0), "all three present");

        // Strike one against #3, then the strike that deletes it: `cached` must still report
        // the three rows the walk examined, not the two it left behind.
        auto_prune(&pool, qid, &keep(&[1, 2])).await;
        let PruneOutcome::Considered {
            cached, deleted, ..
        } = auto_prune_outcome(&pool, qid, &keep(&[1, 2])).await
        else {
            panic!("nothing stamped the query, so the guard cannot have skipped this walk");
        };
        assert_eq!(
            (cached, deleted),
            (3, 1),
            "counted before anything is deleted"
        );
        assert_eq!(remaining_numbers(&pool, qid).await, vec![1, 2]);
    }

    /// The key samples are bounded so one mass departure can't write a thousand keys into a
    /// log line; the counts stay exact, so a truncated list is never mistaken for the whole.
    #[tokio::test]
    async fn prune_caps_the_key_samples_but_not_the_counts() {
        let (pool, _file) = test_pool().await;
        let numbers: Vec<i64> = (1..=(PRUNE_LOG_KEY_CAP as i64 + 5)).collect();
        let qid = query_with_items(&pool, &numbers).await;

        // A forced resync deletes on the first absence, so one call both records and deletes.
        let PruneOutcome::Considered {
            absent,
            deleted,
            absent_keys,
            deleted_keys,
            ..
        } = forced_prune_outcome(&pool, qid, &[]).await
        else {
            panic!("every row is absent from an empty keep set");
        };
        assert_eq!(absent, numbers.len());
        assert_eq!(deleted, numbers.len());
        assert_eq!(absent_keys.len(), PRUNE_LOG_KEY_CAP);
        assert_eq!(deleted_keys.len(), PRUNE_LOG_KEY_CAP);
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

        let deleted = prune_outcome(&pool, qid, &keep(&[1]), 1, before_walk.as_deref())
            .await
            .deleted();
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

        // All three fetch-tracking columns should have been reset → stale, and due for
        // a full fetch so items the *old* query matched get pruned.
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
