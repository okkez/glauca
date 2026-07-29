//! Engine-message handling: the `AppMessage` dispatcher and the view-state
//! synchronization it drives (pending items, unread counts, read marking).

use gpui::*;

use glauca_core::engine::AppMessage;
use glauca_core::logic::*;
use glauca_core::types::*;

use super::*;

impl GlaucaApp {
    /// Install `items` as the visible list. Caller refilters / notifies. `is_new`
    /// (unread) is already set per item by `cached_item_to_item_entry` when the
    /// engine builds them, so there is nothing to recompute here.
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
    /// compute-then-insert split keeps the borrow checker satisfied inside one
    /// `&mut self` method (the compute reads `entries`/`items`, the insert
    /// mutates the map), so callers no longer clone `self.items` just to call
    /// `recompute_unread`. That variant stays for items not yet applied to the
    /// view (ItemsLoaded / apply_pending).
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

    /// Mark the currently-selected item read (it is shown in the detail pane):
    /// record the `updated_at` it was read at, clear its in-memory `is_new`,
    /// recompute the current query's unread badges, and persist via the engine
    /// (fire-and-forget). No-op if there is no selection or the item is already read
    /// (not currently unread).
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
    /// Cache maintenance clears the re-fetchable `body` of old items to save space
    /// (`glauca_core::db::clear_stale_bodies`); a `None` body means "cleared", not
    /// "no description" (an empty description is stored as `Some("")`). Fetch it
    /// once via `RefreshItem`; the reload repopulates the detail pane.
    /// `body_refresh_requested` dedups repeat selection of the same item.
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

    /// Apply a single engine message to GUI state. Mirrors the TUI's `run_app`
    /// message handling (crates/glauca-tui/src/tui/mod.rs). `window` is used to
    /// surface error messages as notification toasts (the status footer is
    /// transient — the next status overwrites it, so errors alone would be easy
    /// to miss).
    pub(crate) fn apply(&mut self, msg: AppMessage, window: &mut Window, cx: &mut Context<Self>) {
        // Only rebuild the filtered-index cache when items/filter/stream_filter
        // actually change. Background sync floods `apply` with messages that don't
        // touch the visible list (other queries' ItemsLoaded, Status, BgSync*,
        // …); recomputing on each would re-scan all items (~thousands)
        // on the UI thread and make the app sluggish while sync runs. Selection and
        // filter edits recompute in their own handlers (`select_index`,
        // `preview_entry`, the filter debounce task).
        let mut needs_refilter = false;
        match msg {
            AppMessage::ItemsLoaded {
                query_id,
                items,
                background,
            } => {
                // Desktop notification, independent of which query is selected:
                // a background sync surfacing new/updated items for any query
                // should notify. Returns `None` on the query's first load this
                // session (baseline only), suppressing the startup storm.
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
                    // Don't change the list under the user. Stash the fresh items
                    // and surface a change banner; applied on explicit action.
                    // Unread badges are deferred too, so nothing moves until then.
                    // Removals count as changes, so a sync that only pruned items
                    // no longer matching the query still surfaces.
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
                // Only foreground (user-driven) failures get a toast. A background
                // worker fault keeps recurring every sync cycle; toasting each one
                // would bury the notification layer, so those stay in the status
                // line only.
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

            // ── Entry add / edit / delete / reorder (mirror TUI mod.rs) ─────────
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
            AppMessage::QueriesSwapped {
                upper_id,
                active_id,
                ..
            } => {
                if let Some(idx) = self
                    .entries
                    .iter()
                    .position(|e| matches!(e, LeftPaneEntry::Query(q) if q.id == upper_id))
                {
                    move_group_down(&mut self.entries, idx);
                }
                if let Some(pos) = self.entries.iter().position(|e| e.id() == active_id) {
                    self.entry_cursor = pos;
                }
            }
            AppMessage::FilterStreamsSwapped {
                upper_id,
                lower_id,
                active_id,
            } => {
                let u = self.entries.iter().position(|e| e.id() == upper_id);
                let l = self.entries.iter().position(|e| e.id() == lower_id);
                if let (Some(u), Some(l)) = (u, l) {
                    self.entries.swap(u, l);
                }
                if let Some(pos) = self.entries.iter().position(|e| e.id() == active_id) {
                    self.entry_cursor = pos;
                }
            }

            // ── Action results ──────────────────────────────────────────────────
            AppMessage::ActionDone(s) => self.status = Some(s),
            AppMessage::ActionError(e) => {
                self.status = Some(format!("Error: {e}"));
                window.push_notification(Notification::error(e), cx);
            }

            // ── Comments overlay ────────────────────────────────────────────────
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
        }
        if needs_refilter {
            self.recompute_filtered();
        }
    }
}
