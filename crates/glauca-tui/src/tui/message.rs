//! Handling of `AppMessage`s drained from the engine in the run loop: applying
//! loaded items (with the background change banner), left-pane entry
//! add/update/delete/reorder confirmations, sync status, and comment loads.

use super::*;

pub(crate) async fn handle_app_message(app: &mut App, engine: &Engine, msg: AppMessage) {
    match msg {
        AppMessage::ItemsLoaded {
            query_id,
            items,
            background,
        } => {
            // Independent of which query is selected. Returns `None` on that query's first
            // load this session, suppressing the startup storm.
            let to_notify = app
                .notif_tracker
                .changed_count_to_notify(query_id, &items, background, app.notifications_enabled)
                .and_then(|n| query_label(&app.entries, query_id).map(|name| (name, n)));
            if let Some((name, n)) = to_notify {
                tokio::task::spawn_blocking(move || {
                    glauca_core::notify::notify_updated_items(&name, n)
                });
            }
            let is_current = app.selected_root_query_id() == Some(query_id);
            if is_current && background {
                // Don't change the list under the user: stash the fresh items behind a
                // banner, applied via `u`. Removals count too, so a sync that only pruned
                // still surfaces.
                let changes = glauca_core::logic::count_changes(&app.items, &items);
                if changes.is_empty() {
                    app.clear_pending();
                } else {
                    app.pending_items = Some(items);
                    app.pending_changes = changes;
                }
            } else {
                app.recompute_unread_counts_for_query(query_id, &items);
                if is_current {
                    // Foreground load: apply live and drop any banner.
                    app.apply_items_to_view(items);
                    app.clear_pending();
                }
            }
        }
        AppMessage::QueryAdded(q) => {
            app.entries.push(LeftPaneEntry::Query(q));
            app.entry_cursor = app.entries.len() - 1;
            app.clear_items();
            app.filter = SingleLineInput::new();
            app.stream_filter = None;
            select_current_entry(app, engine, true).await;
        }
        AppMessage::FilterStreamAdded(fs) => {
            // Insert the filter stream after the last sibling (or after its parent)
            let insert_pos = app
                .entries
                .iter()
                .rposition(|e| e.root_query_id() == fs.parent_id)
                .map(|p| p + 1)
                .unwrap_or(app.entries.len());
            app.entries
                .insert(insert_pos, LeftPaneEntry::FilterStream(fs));
            // Select the newly added filter stream
            app.entry_cursor = insert_pos;
            app.filter = SingleLineInput::new();
            app.item_cursor = 0;
            app.detail_scroll = 0;
            select_current_entry(app, engine, true).await;
        }
        AppMessage::QueryUpdated {
            id,
            new_name,
            new_query,
        } => {
            if let Some(LeftPaneEntry::Query(q)) = app
                .entries
                .iter_mut()
                .find(|e| matches!(e, LeftPaneEntry::Query(q) if q.id == id))
            {
                q.label = new_name.clone().unwrap_or_else(|| new_query.clone());
                q.query_str = new_query.clone();
            }
            // Reload + sync with the new query string
            if app.selected_root_query_id() == Some(id) {
                app.clear_items();
                app.item_cursor = 0;
                app.detail_scroll = 0;
                app.filter = SingleLineInput::new();
                engine
                    .send(EngineCommand::LoadCached { query_id: id })
                    .await;
                engine
                    .send(EngineCommand::Sync {
                        query_id: id,
                        query_str: new_query,
                    })
                    .await;
                app.syncing = true;
            }
            app.status = Some("Query updated".into());
        }
        AppMessage::FilterStreamUpdated {
            id,
            new_name,
            new_filter,
        } => {
            if let Some(LeftPaneEntry::FilterStream(fs)) = app
                .entries
                .iter_mut()
                .find(|e| matches!(e, LeftPaneEntry::FilterStream(fs) if fs.id == id))
            {
                fs.name = new_name;
                fs.filter = new_filter.clone();
            }
            // If this filter stream is currently selected, re-apply its filter
            if let Some(LeftPaneEntry::FilterStream(fs)) = app.entries.get(app.entry_cursor)
                && fs.id == id
            {
                app.stream_filter = Some(new_filter.clone());
                app.item_cursor = 0;
                app.detail_scroll = 0;
                app.clamp_item_cursor();
            }
            if let Some(root_id) = app.entries.iter().find_map(|entry| match entry {
                LeftPaneEntry::FilterStream(fs) if fs.id == id => Some(fs.parent_id),
                _ => None,
            }) {
                let items = app.items.clone();
                app.recompute_unread_counts_for_query(root_id, &items);
            }
            app.status = Some("Filter stream updated".into());
        }
        AppMessage::Status(s) => {
            app.status = Some(s);
        }
        AppMessage::ActionDone(msg) => {
            app.status = Some(msg);
        }
        AppMessage::ActionError(err) => {
            // Also the failure path for a reorder round trip (both the reorder and the
            // read-back failed, so no EntriesReloaded followed) — clear the gate here too,
            // or a rejected reorder would leave J/K dead for the rest of the session.
            app.reorder_pending = false;
            app.status = Some(format!("Error: {err}"));
        }
        AppMessage::CommentsLoaded(comments) => {
            app.comments = comments;
            app.comments_loading = false;
        }
        AppMessage::CommentsFailed(err) => {
            app.comments_loading = false;
            // Stay in CommentsPopup so the user sees the error; show it as a comment
            app.comments = vec![CommentEntry {
                author: "error".into(),
                created_at: String::new(),
                body: format!("Failed to load comments: {err}"),
                is_minimized: false,
                minimized_reason: None,
            }];
        }
        AppMessage::SyncDone { query_id, count } => {
            if app.selected_root_query_id() == Some(query_id) {
                app.syncing = false;
                app.status = Some(format!("Synced {count} items"));
            }
        }
        AppMessage::SyncError {
            query_id, error, ..
        } => {
            if app.selected_root_query_id() == Some(query_id) {
                app.syncing = false;
            }
            app.status = Some(format!("Sync error: {error}"));
        }
        AppMessage::BgSyncQueued(n) => {
            app.bg_sync_pending += n;
        }
        AppMessage::BgSyncJobDone => {
            app.bg_sync_pending = app.bg_sync_pending.saturating_sub(1);
        }
        AppMessage::SyncStarted { query_id } => {
            if app.selected_root_query_id() == Some(query_id) {
                app.syncing = true;
            }
        }
        AppMessage::QueryDeleted { query_id } => {
            // Remove all entries for this root query and its streams.
            app.entries.retain(|e| e.root_query_id() != query_id);
            app.entry_cursor = app.entry_cursor.min(app.entries.len().saturating_sub(1));
            app.clear_items();
            app.item_cursor = 0;
            app.filter = SingleLineInput::new();
            app.stream_filter = None;
            select_current_entry(app, engine, true).await;
        }
        AppMessage::FilterStreamDeleted { id } => {
            app.entries.retain(|e| e.id() != id);
            app.entry_cursor = app.entry_cursor.min(app.entries.len().saturating_sub(1));
            app.clear_items();
            app.item_cursor = 0;
            app.filter = SingleLineInput::new();
            app.stream_filter = None;
            select_current_entry(app, engine, true).await;
        }
        AppMessage::EntriesReloaded { entries, active } => {
            app.reorder_pending = false;
            let previous = app.entries.get(app.entry_cursor).map(|e| e.key());
            let (cursor, changed) = glauca_core::logic::resolve_reloaded_selection(
                &entries,
                active,
                previous,
                app.entry_cursor,
            );
            app.entries = entries;
            app.entry_cursor = cursor;
            if changed {
                // The reload lands on a different entry than was selected before this
                // message (deleted by another instance, a reorder whose `active` names a
                // row other than the previous selection, or the cursor having moved while
                // this round trip was in flight). `items`/`stream_filter` still belong to
                // the old selection, so follow the cursor to the new one rather than
                // leaving the item pane showing it under a different highlight.
                app.clear_items();
                app.item_cursor = 0;
                app.detail_scroll = 0;
                app.filter = SingleLineInput::new();
                app.stream_filter = None;
                select_current_entry(app, engine, true).await;
            }
        }
        AppMessage::CurrentUserResolved { login, .. } => app.adopt_current_user(login),
    }
}
