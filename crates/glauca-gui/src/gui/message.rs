//! Engine-message handling: the `AppMessage` dispatcher and the view-state
//! synchronization it drives (pending items, unread counts, read marking).

use gpui::*;

use glauca_core::engine::AppMessage;
use glauca_core::logic::*;
use glauca_core::types::*;

use super::*;

impl GlaucaApp {
    /// Install `items` as the visible list. Caller refilters / notifies. `is_new` is
    /// already set per item by `cached_item_to_item_entry`, so nothing is recomputed here.
    pub(crate) fn apply_items_to_view(&mut self, items: Vec<ItemEntry>) {
        self.items = items;
    }

    /// Drop any held-back background-sync results / banner.
    pub(crate) fn clear_pending(&mut self) {
        self.pending_items = None;
        self.pending_changes = ChangeCounts::default();
    }

    /// Apply the stashed background-sync results to the visible list (banner
    /// click / explicit refresh). No-op when nothing is pending.
    pub(crate) fn apply_pending(&mut self, cx: &mut Context<Self>) {
        let Some(items) = self.pending_items.take() else {
            return;
        };
        self.pending_changes = ChangeCounts::default();
        if let Some(qid) = self.selected_root_query_id() {
            self.recompute_unread(qid, &items);
        }
        self.apply_items_to_view(items);
        self.recompute_filtered();
        cx.notify();
    }

    pub(crate) fn recompute_unread(&mut self, query_id: i64, items: &[ItemEntry]) {
        for (key, unread) in
            compute_unread_counts(&self.entries, query_id, items, self.current_user.as_deref())
        {
            self.unread_counts.insert(key, unread);
        }
    }

    /// Recompute unread badges for `query_id` from the live `self.items`. The
    /// compute-then-insert split keeps the borrow checker satisfied inside one `&mut self`
    /// method. `recompute_unread` is the variant for items not yet applied to the view.
    pub(crate) fn recompute_unread_live(&mut self, query_id: i64) {
        let updates = compute_unread_counts(
            &self.entries,
            query_id,
            &self.items,
            self.current_user.as_deref(),
        );
        for (key, unread) in updates {
            self.unread_counts.insert(key, unread);
        }
    }

    /// Mark the currently-selected item read: record the `updated_at` it was read at,
    /// clear its in-memory `is_new`, recompute the query's unread badges, and persist via
    /// the engine (fire-and-forget). No-op without a selection or on an already-read item.
    pub(crate) fn mark_current_item_read(&mut self, cx: &mut Context<Self>) {
        let Some(&idx) = self.filtered.get(self.item_cursor) else {
            return;
        };
        let Some(row) = self.items.get_mut(idx) else {
            return;
        };
        if !is_item_unread(&row.updated_at, row.last_read_updated_at.as_deref()) {
            return;
        }
        row.last_read_updated_at = Some(row.updated_at.clone());
        row.is_new = false;
        let (repo_owner, repo_name, number) =
            (row.repo_owner.clone(), row.repo_name.clone(), row.number);
        if let Some(query_id) = self.selected_root_query_id() {
            self.recompute_unread_live(query_id);
            self.send(EngineCommand::MarkItemRead {
                query_id,
                repo_owner,
                repo_name,
                number,
            });
        }
        cx.notify();
    }

    /// Transparently re-fetch the body of the viewed item when it is missing.
    ///
    /// Cache maintenance clears the re-fetchable `body` of old items, so a `None` body
    /// means "cleared", not "no description" — an empty description is stored as
    /// `Some("")`. Fetched once via `RefreshItem`; `body_refresh_requested` dedups repeat
    /// selection of the same item.
    pub(crate) fn refetch_current_body_if_missing(&mut self) {
        let Some(&idx) = self.filtered.get(self.item_cursor) else {
            return;
        };
        let Some(row) = self.items.get(idx) else {
            return;
        };
        if row.body.is_some() {
            return;
        }
        let key = (row.repo_owner.clone(), row.repo_name.clone(), row.number);
        if self.body_refresh_requested.contains(&key) {
            return;
        }
        let Some(query_id) = self.selected_root_query_id() else {
            return;
        };
        self.body_refresh_requested.insert(key.clone());
        self.send(EngineCommand::RefreshItem {
            query_id,
            repo_owner: key.0,
            repo_name: key.1,
            number: key.2,
        });
    }

    /// Apply a single engine message to GUI state. `window` is used to surface errors as
    /// notification toasts: the status footer is transient, so the next status would
    /// overwrite an error before it is read.
    pub(crate) fn apply(&mut self, msg: AppMessage, window: &mut Window, cx: &mut Context<Self>) {
        // Only rebuild the filtered-index cache when items/filter/stream_filter actually
        // change. Background sync floods `apply` with messages that don't touch the
        // visible list, and recomputing on each would re-scan thousands of items on the UI
        // thread. Selection and filter edits recompute in their own handlers.
        let mut needs_refilter = false;
        match msg {
            AppMessage::ItemsLoaded {
                query_id,
                items,
                background,
            } => {
                // Independent of which query is selected: a background sync surfacing new
                // items for any query should notify. Returns `None` on that query's first
                // load this session, suppressing the startup storm.
                let to_notify = self
                    .notif_tracker
                    .changed_count_to_notify(
                        query_id,
                        &items,
                        background,
                        self.settings.notifications_enabled,
                    )
                    .and_then(|n| query_label(&self.entries, query_id).map(|name| (name, n)));
                if let Some((name, n)) = to_notify {
                    cx.background_executor()
                        .spawn(async move { glauca_core::notify::notify_updated_items(&name, n) })
                        .detach();
                }
                let is_current = self.selected_root_query_id() == Some(query_id);
                if is_current && background {
                    // Don't change the list under the user: stash the fresh items behind a
                    // banner, applied on explicit action. Unread badges are deferred too.
                    // Removals count as changes, so a sync that only pruned still surfaces.
                    let changes = count_changes(&self.items, &items);
                    if changes.is_empty() {
                        self.clear_pending();
                    } else {
                        self.pending_items = Some(items);
                        self.pending_changes = changes;
                    }
                } else {
                    self.recompute_unread(query_id, &items);
                    if is_current {
                        // Foreground load for the current query: apply live and drop
                        // any banner (this load supersedes it).
                        self.apply_items_to_view(items);
                        self.clear_pending();
                        needs_refilter = true;
                    }
                }
            }
            AppMessage::SyncStarted { .. } => self.syncing = true,
            AppMessage::SyncDone { count, .. } => {
                self.syncing = false;
                self.status = Some(format!("Synced {count} items"));
            }
            AppMessage::SyncError {
                error, background, ..
            } => {
                self.syncing = false;
                self.status = Some(format!("Sync error: {error}"));
                // Only foreground failures get a toast. A background worker fault recurs
                // every sync cycle, and toasting each would bury the notification layer.
                if !background {
                    window
                        .push_notification(Notification::error(format!("Sync error: {error}")), cx);
                }
            }
            AppMessage::BgSyncQueued(n) => self.bg_sync_pending += n,
            AppMessage::BgSyncJobDone => {
                self.bg_sync_pending = self.bg_sync_pending.saturating_sub(1);
            }
            AppMessage::Status(s) => self.status = Some(s),

            AppMessage::QueryAdded(q) => {
                self.entries.push(LeftPaneEntry::Query(q));
                self.entry_cursor = self.entries.len() - 1;
                self.filter.clear();
                self.select_index(self.entry_cursor);
            }
            AppMessage::FilterStreamAdded(fs) => {
                let insert_pos = self
                    .entries
                    .iter()
                    .rposition(|e| e.root_query_id() == fs.parent_id)
                    .map(|p| p + 1)
                    .unwrap_or(self.entries.len());
                self.entries
                    .insert(insert_pos, LeftPaneEntry::FilterStream(fs));
                self.entry_cursor = insert_pos;
                self.filter.clear();
                self.select_index(self.entry_cursor);
            }
            AppMessage::QueryUpdated {
                id,
                new_name,
                new_query,
            } => {
                if let Some(LeftPaneEntry::Query(q)) = self
                    .entries
                    .iter_mut()
                    .find(|e| matches!(e, LeftPaneEntry::Query(q) if q.id == id))
                {
                    q.label = new_name.clone().unwrap_or_else(|| new_query.clone());
                    q.query_str = new_query.clone();
                }
                if self.selected_root_query_id() == Some(id) {
                    self.items.clear();
                    self.item_cursor = 0;
                    self.filter.clear();
                    self.send(EngineCommand::LoadCached { query_id: id });
                    self.send(EngineCommand::Sync {
                        query_id: id,
                        query_str: new_query,
                    });
                    self.syncing = true;
                    needs_refilter = true;
                }
                self.status = Some("Query updated".into());
            }
            AppMessage::FilterStreamUpdated {
                id,
                new_name,
                new_filter,
            } => {
                let mut root_id = None;
                if let Some(LeftPaneEntry::FilterStream(fs)) = self
                    .entries
                    .iter_mut()
                    .find(|e| matches!(e, LeftPaneEntry::FilterStream(fs) if fs.id == id))
                {
                    fs.name = new_name;
                    fs.filter = new_filter.clone();
                    root_id = Some(fs.parent_id);
                }
                if matches!(self.entries.get(self.entry_cursor), Some(LeftPaneEntry::FilterStream(fs)) if fs.id == id)
                {
                    self.stream_filter = Some(new_filter);
                    self.item_cursor = 0;
                    needs_refilter = true;
                }
                if let Some(root_id) = root_id {
                    self.recompute_unread_live(root_id);
                }
                self.status = Some("Filter stream updated".into());
            }
            AppMessage::QueryDeleted { query_id } => {
                needs_refilter |=
                    self.remove_entries_and_reselect(|e| e.root_query_id() != query_id);
            }
            AppMessage::FilterStreamDeleted { id } => {
                needs_refilter |= self.remove_entries_and_reselect(|e| e.id() != id);
            }
            AppMessage::EntriesReloaded { entries, active } => {
                self.reorder_pending = false;
                let previous = self.entries.get(self.entry_cursor).map(|e| e.key());
                let (cursor, changed) =
                    resolve_reloaded_selection(&entries, active, previous, self.entry_cursor);
                self.entries = entries;
                if changed {
                    // The previously selected entry is gone from the reload — it was
                    // deleted by another instance between the keypress and this read-back,
                    // one of the two ways a reorder can be rejected. `items`/`stream_filter`
                    // still belong to that entry, so follow the cursor to the new selection
                    // rather than leaving the item pane showing it under a different
                    // highlight.
                    self.select_index(cursor);
                } else {
                    self.entry_cursor = cursor;
                }
            }

            AppMessage::ActionDone(s) => self.status = Some(s),
            AppMessage::ActionError(e) => {
                // Also the failure path for a reorder round trip (both the reorder and the
                // read-back failed, so no EntriesReloaded followed) — clear the gate here
                // too, or a rejected reorder would leave reordering dead for the session.
                self.reorder_pending = false;
                self.status = Some(format!("Error: {e}"));
                window.push_notification(Notification::error(e), cx);
            }

            AppMessage::CommentsLoaded(comments) => {
                if self.comments_open {
                    self.comments = comments;
                    self.comments_loading = false;
                }
            }
            AppMessage::CommentsFailed(e) => {
                self.comments_loading = false;
                self.status = Some(format!("Failed to load comments: {e}"));
                window.push_notification(
                    Notification::error(format!("Failed to load comments: {e}")),
                    cx,
                );
            }

            // Everything computed against `@me` so far answered the wrong question —
            // matching nobody, or everybody for a negated `@me` — so redo the visible list
            // and the selected query's badges. Other queries correct themselves on load.
            AppMessage::CurrentUserResolved {
                login,
                name,
                avatar_url,
            } => {
                self.status = Some(format!("Signed in as {login}"));
                self.current_user = Some(login);
                self.current_user_name = name;
                self.current_user_avatar_url = avatar_url;
                if let Some(query_id) = self.selected_root_query_id() {
                    self.recompute_unread_live(query_id);
                }
                needs_refilter = true;
            }
        }
        if needs_refilter {
            self.recompute_filtered();
        }
    }
}
