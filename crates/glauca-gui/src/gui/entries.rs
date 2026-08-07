//! Left-pane entry operations: delete, reorder, mark-all-read, the item
//! filter recompute, and the select/resync flow when the current entry changes.

use gpui::*;

use glauca_core::engine::EngineCommand;
use glauca_core::filter::{FilterQuery, StreamFilter};
use glauca_core::logic::*;
use glauca_core::types::*;

use super::*;

impl GlaucaApp {
    pub(crate) fn on_delete_entry(
        &mut self,
        _: &DeleteEntry,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        if self.focus != Focus::QueryList {
            return;
        }
        self.delete_entry_at(self.entry_cursor);
    }

    /// Delete the entry at `index` (query or filter stream). UI updates when the
    /// QueryDeleted/FilterStreamDeleted confirmation arrives.
    pub(crate) fn delete_entry_at(&mut self, index: usize) {
        let cmd = match self.entries.get(index) {
            Some(LeftPaneEntry::Query(q)) => Some(EngineCommand::DeleteQuery { query_id: q.id }),
            Some(LeftPaneEntry::FilterStream(fs)) => {
                Some(EngineCommand::DeleteFilterStream { id: fs.id })
            }
            None => None,
        };
        if let Some(cmd) = cmd {
            self.send(cmd);
        }
    }

    /// Mark every unread item of the entry at `index` read — the whole root query, or a
    /// filter stream's matching items with the filter expanded here, since the engine does
    /// not know `@me`. The engine reloads the query afterwards, which refreshes the badges
    /// through `ItemsLoaded`, so this works for non-selected entries too.
    pub(crate) fn mark_all_read_at(&mut self, index: usize) {
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let query_id = entry.root_query_id();
        let filter = entry
            .stream_filter()
            .map(|f| expand_me(self.current_user.as_deref(), f).into_owned());
        self.send(EngineCommand::MarkAllRead { query_id, filter });
    }

    pub(crate) fn on_reorder_down(
        &mut self,
        _: &ReorderDown,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.reorder(true);
    }

    pub(crate) fn on_reorder_up(
        &mut self,
        _: &ReorderUp,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.reorder(false);
    }

    /// Move the selected entry up/down within its group. Sends a swap command; the entries
    /// vec is reordered only when the *Swapped confirmation arrives.
    pub(crate) fn reorder(&mut self, down: bool) {
        if self.focus != Focus::QueryList {
            return;
        }
        self.reorder_entry(self.entry_cursor, down);
    }

    /// Move the entry at `cursor` up/down within its group (index-based; no focus
    /// guard so right-click can target any row).
    pub(crate) fn reorder_entry(&mut self, cursor: usize, down: bool) {
        let cmd = match self.entries.get(cursor) {
            Some(LeftPaneEntry::Query(q)) => {
                let current_id = q.id;
                if down {
                    let next_query_idx = group_range(&self.entries, cursor).end;
                    match self.entries.get(next_query_idx) {
                        Some(LeftPaneEntry::Query(nq)) => Some(EngineCommand::SwapQueryPositions {
                            upper_id: current_id,
                            lower_id: nq.id,
                            active_id: current_id,
                        }),
                        _ => None,
                    }
                } else {
                    self.entries[..cursor]
                        .iter()
                        .rposition(|e| matches!(e, LeftPaneEntry::Query(_)))
                        .and_then(|prev_idx| match &self.entries[prev_idx] {
                            LeftPaneEntry::Query(pq) => Some(EngineCommand::SwapQueryPositions {
                                upper_id: pq.id,
                                lower_id: current_id,
                                active_id: current_id,
                            }),
                            _ => None,
                        })
                }
            }
            Some(LeftPaneEntry::FilterStream(fs)) => {
                let fs_id = fs.id;
                let parent_id = fs.parent_id;
                if down {
                    match self.entries.get(cursor + 1) {
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
                    match self.entries.get(cursor - 1) {
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
            None => None,
        };
        if let Some(cmd) = cmd {
            self.send(cmd);
        }
    }

    pub(crate) fn filtered_len(&self) -> usize {
        self.filtered.len()
    }

    /// Whether the filters shaping the current view lean on `@me` while the login is
    /// unknown — the list is then wrong for a reason it cannot show, and the status footer
    /// turns this into a warning. See [`glauca_core::logic::has_unexpanded_me`].
    pub(crate) fn has_unexpanded_me(&self) -> bool {
        glauca_core::logic::has_unexpanded_me(
            self.current_user.as_deref(),
            self.stream_filter.as_deref(),
            &self.filter,
        )
    }

    /// Rebuild the `filtered` index cache from `items` + stream/inline filters, yielding
    /// indices so render can reuse them without re-scanning every frame. Call after any
    /// change to `items`, `filter`, or `stream_filter`.
    pub(crate) fn recompute_filtered(&mut self) {
        let stream_q = self
            .stream_filter
            .as_deref()
            .map(|s| StreamFilter::parse(s, self.current_user.as_deref()));
        let inline_q = FilterQuery::parse(&expand_me(self.current_user.as_deref(), &self.filter));
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                stream_q.as_ref().is_none_or(|q| q.matches(item))
                    && (inline_q.is_empty() || inline_q.matches(item))
            })
            .map(|(ix, _)| ix)
            .collect();
        if self.item_cursor >= self.filtered.len() {
            self.item_cursor = self.filtered.len().saturating_sub(1);
        }
        // Keep the virtualized list's item count in step with `filtered`, or `list` panics
        // or renders stale rows. Every `filtered` mutation flows through here.
        self.items_list.reset(self.filtered.len());
    }

    /// Issue the engine commands to (re)load the currently selected entry. Returns the
    /// root query id when a query (not a filter stream) was selected, so the caller can
    /// skip it in the background-refresh sweep.
    pub(crate) fn select_current_entry(&mut self, always_sync: bool) -> Option<i64> {
        let entry = self.entries.get(self.entry_cursor)?.clone();
        // Selecting a query does NOT mark it viewed: unread is derived per item from
        // `updated_at` vs `last_read_updated_at`, so there is no per-entry baseline.
        self.stream_filter = entry.stream_filter().map(|s| s.to_string());

        let root_id = entry.root_query_id();
        self.send(EngineCommand::LoadCached { query_id: root_id });
        if entry.is_filter_stream() {
            return None;
        }

        // A root query must carry its query string; an empty one would fire a
        // pointless (and confusing) blank GitHub search, so skip the sync.
        let query_str = match entry.root_query_str() {
            Some(q) if !q.is_empty() => q.to_string(),
            _ => {
                tracing::warn!(query_id = root_id, "root query has no query string");
                return None;
            }
        };
        if always_sync {
            self.send(EngineCommand::Sync {
                query_id: root_id,
                query_str,
            });
            self.syncing = true;
        } else {
            self.send(EngineCommand::SyncIfStale {
                query_id: root_id,
                query_str,
            });
        }
        Some(root_id)
    }

    /// Force a full re-fetch of the current entry's root query, ignoring
    /// `last_fetched_at`: re-pages everything and prunes cached items that no longer
    /// match, e.g. merged PRs lingering in an `is:open` list.
    pub(crate) fn full_resync_current(&mut self) {
        let Some(entry) = self.entries.get(self.entry_cursor) else {
            return;
        };
        let root_id = entry.root_query_id();
        let query_str = entry.root_query_str().unwrap_or_default().to_string();
        if query_str.is_empty() {
            return;
        }
        self.send(EngineCommand::FullResync {
            query_id: root_id,
            query_str,
        });
        self.syncing = true;
    }

    /// Shared tail of the QueryDeleted / FilterStreamDeleted arms: drop the entries
    /// rejected by `retain`, clamp the cursor, clear the inline filter, and either
    /// reselect or empty the view. Returns true when the caller must refilter.
    pub(crate) fn remove_entries_and_reselect(
        &mut self,
        retain: impl Fn(&LeftPaneEntry) -> bool,
    ) -> bool {
        self.entries.retain(retain);
        if self.entry_cursor >= self.entries.len() {
            self.entry_cursor = self.entries.len().saturating_sub(1);
        }
        self.filter.clear();
        if self.entries.is_empty() {
            self.items.clear();
            self.stream_filter = None;
            true
        } else {
            self.select_index(self.entry_cursor);
            false
        }
    }
}
