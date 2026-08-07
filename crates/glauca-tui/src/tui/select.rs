//! Left-pane selection plumbing: reordering entries within their group, and (re)loading the
//! items backing the selected entry. These only send engine commands; the entries vec and
//! item list are updated later, when the engine confirms with the matching `AppMessage`.

use super::*;

// 非同期タスク（load_items_task/sync_task/sync_worker_task 等）は glauca_core::engine にある。

/// Position-swap command for moving the entry at `cursor` up (`down=false`) or
/// down within its group: a query swaps with the adjacent query group, a filter
/// stream with an adjacent sibling under the same parent. `None` if there is no
/// neighbor to swap with. The entries vec is reordered later, when the engine
/// confirms with QueriesSwapped / FilterStreamsSwapped.
pub(crate) fn reorder_command(
    entries: &[LeftPaneEntry],
    cursor: usize,
    down: bool,
) -> Option<EngineCommand> {
    match entries.get(cursor)? {
        LeftPaneEntry::Query(q) => {
            let current_id = q.id;
            if down {
                let next_query_idx = group_range(entries, cursor).end;
                match entries.get(next_query_idx)? {
                    LeftPaneEntry::Query(nq) => Some(EngineCommand::SwapQueryPositions {
                        upper_id: current_id,
                        lower_id: nq.id,
                        active_id: current_id,
                    }),
                    _ => None,
                }
            } else {
                let prev_idx = entries[..cursor]
                    .iter()
                    .rposition(|e| matches!(e, LeftPaneEntry::Query(_)))?;
                match &entries[prev_idx] {
                    LeftPaneEntry::Query(pq) => Some(EngineCommand::SwapQueryPositions {
                        upper_id: pq.id,
                        lower_id: current_id,
                        active_id: current_id,
                    }),
                    _ => None,
                }
            }
        }
        LeftPaneEntry::FilterStream(fs) => {
            let fs_id = fs.id;
            let parent_id = fs.parent_id;
            if down {
                // Swap with next sibling (same parent, immediately after).
                match entries.get(cursor + 1) {
                    Some(LeftPaneEntry::FilterStream(next)) if next.parent_id == parent_id => {
                        Some(EngineCommand::SwapFilterStreamPositions {
                            upper_id: fs_id,
                            lower_id: next.id,
                            active_id: fs_id,
                        })
                    }
                    _ => None,
                }
            } else if cursor > 0 {
                // Swap with previous sibling (same parent, immediately before).
                match entries.get(cursor - 1) {
                    Some(LeftPaneEntry::FilterStream(prev)) if prev.parent_id == parent_id => {
                        Some(EngineCommand::SwapFilterStreamPositions {
                            upper_id: prev.id,
                            lower_id: fs_id,
                            active_id: fs_id,
                        })
                    }
                    _ => None,
                }
            } else {
                None
            }
        }
    }
}

/// What `prepare_selected_entry_load` resolves for the selected left-pane entry:
/// the root query to load items for, plus how to interpret/highlight them.
struct SelectedEntryLoad {
    root_id: i64,
    query_str: Option<String>,
    is_filter_stream: bool,
}

fn prepare_selected_entry_load(app: &mut App) -> Option<SelectedEntryLoad> {
    let entry = app.entries.get(app.entry_cursor)?.clone();
    // Selecting an entry does NOT mark it viewed: badges clear per item as items are read
    // (see `mark_selected_item_read`). Only the stream filter is updated here.
    app.stream_filter = entry.stream_filter().map(|s| s.to_string());
    // Switching entries invalidates any held-back update for the previous one.
    app.clear_pending();

    Some(SelectedEntryLoad {
        root_id: entry.root_query_id(),
        query_str: entry.root_query_str().map(str::to_string),
        is_filter_stream: entry.is_filter_stream(),
    })
}

/// Issue the engine commands to (re)load the currently selected entry. With `always_sync`,
/// sync unconditionally and show the indicator immediately; otherwise only if the cache is
/// stale. Returns the root query id for a query, so the caller can skip it in the sweep.
pub(crate) async fn select_current_entry(
    app: &mut App,
    engine: &Engine,
    always_sync: bool,
) -> Option<i64> {
    let load = prepare_selected_entry_load(app)?;
    engine
        .send(EngineCommand::LoadCached {
            query_id: load.root_id,
        })
        .await;
    if load.is_filter_stream {
        return None;
    }
    let query_str = load.query_str.clone().unwrap_or_default();
    if always_sync {
        engine
            .send(EngineCommand::Sync {
                query_id: load.root_id,
                query_str,
            })
            .await;
        app.syncing = true;
    } else {
        engine
            .send(EngineCommand::SyncIfStale {
                query_id: load.root_id,
                query_str,
            })
            .await;
    }
    Some(load.root_id)
}

/// The query string of the root query with `root_id` — needed to re-sync the list backing
/// a filter stream, which has no query string of its own.
fn root_query_str(app: &App, root_id: i64) -> Option<String> {
    app.entries.iter().find_map(|e| match e {
        LeftPaneEntry::Query(q) if q.id == root_id => Some(q.query_str.clone()),
        _ => None,
    })
}

/// Re-sync the list for the currently selected entry (its root query) without
/// resetting the cursor/scroll, so a manual refresh keeps the user's place.
pub(crate) async fn refresh_selected_list(app: &mut App, engine: &Engine) {
    let Some(root_id) = app.entries.get(app.entry_cursor).map(|e| e.root_query_id()) else {
        return;
    };
    let Some(query_str) = root_query_str(app, root_id) else {
        app.status = Some("Nothing to refresh".into());
        return;
    };
    engine
        .send(EngineCommand::Sync {
            query_id: root_id,
            query_str,
        })
        .await;
    app.syncing = true;
}

/// Force a full re-fetch of the selected entry's root query (ignores
/// `last_fetched_at`): re-pages everything and prunes cached items that no longer
/// match the query.
pub(crate) async fn full_resync_selected(app: &mut App, engine: &Engine) {
    let Some(root_id) = app.entries.get(app.entry_cursor).map(|e| e.root_query_id()) else {
        return;
    };
    let Some(query_str) = root_query_str(app, root_id) else {
        app.status = Some("Nothing to resync".into());
        return;
    };
    engine
        .send(EngineCommand::FullResync {
            query_id: root_id,
            query_str,
        })
        .await;
    app.syncing = true;
}

/// Re-fetch just the selected item from GitHub into its query's cache.
pub(crate) async fn refresh_selected_item(app: &mut App, engine: &Engine) {
    let Some(item) = app.selected_item().cloned() else {
        return;
    };
    let Some(query_id) = app.selected_root_query_id() else {
        return;
    };
    engine
        .send(EngineCommand::RefreshItem {
            query_id,
            repo_owner: item.repo_owner.clone(),
            repo_name: item.repo_name.clone(),
            number: item.number,
        })
        .await;
    app.status = Some(format!("Refreshing #{}…", item.number));
}

/// Transparently re-fetch the body of the viewed item when it is missing.
///
/// Cache maintenance clears the re-fetchable `body` of old items, so a `None` body means
/// "cleared", not "no description" — an empty description is stored as `Some("")`. Fetched
/// once via `RefreshItem`; `body_refresh_requested` dedups, since the caller runs on every
/// keypress.
pub(crate) async fn refetch_selected_body_if_missing(app: &mut App, engine: &Engine) {
    let Some(item) = app.selected_item() else {
        return;
    };
    if item.body.is_some() {
        return;
    }
    let key = (item.repo_owner.clone(), item.repo_name.clone(), item.number);
    if app.body_refresh_requested.contains(&key) {
        return;
    }
    let Some(query_id) = app.selected_root_query_id() else {
        return;
    };
    app.body_refresh_requested.insert(key.clone());
    engine
        .send(EngineCommand::RefreshItem {
            query_id,
            repo_owner: key.0,
            repo_name: key.1,
            number: key.2,
        })
        .await;
}

/// Mark the item under the cursor read: record the `updated_at` it was read at, clear its
/// in-memory `is_new`, recompute the query's unread badges, and persist via the engine
/// (fire-and-forget). No-op without a selection or on an already-read item.
pub(crate) async fn mark_selected_item_read(app: &mut App, engine: &Engine) {
    let Some(item) = app.selected_item().cloned() else {
        return;
    };
    let Some(idx) = app.items.iter().position(|i| {
        i.repo_owner == item.repo_owner && i.repo_name == item.repo_name && i.number == item.number
    }) else {
        return;
    };
    let row = &mut app.items[idx];
    if !is_item_unread(&row.updated_at, row.last_read_updated_at.as_deref()) {
        return;
    }
    row.last_read_updated_at = Some(row.updated_at.clone());
    row.is_new = false;
    let Some(query_id) = app.selected_root_query_id() else {
        return;
    };
    // Recompute from the live items (compute → insert, to avoid borrowing app
    // mutably while reading app.items/entries).
    let updates = glauca_core::logic::compute_unread_counts(
        &app.entries,
        query_id,
        &app.items,
        app.current_user.as_deref(),
    );
    for (key, unread) in updates {
        app.unread_counts.insert(key, unread);
    }
    engine
        .send(EngineCommand::MarkItemRead {
            query_id,
            repo_owner: item.repo_owner,
            repo_name: item.repo_name,
            number: item.number,
        })
        .await;
}
