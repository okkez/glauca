//! Navigation and focus: engine-command send helpers, selection/preview, and
//! the j/k/h/l/enter/`/`/Esc key handlers that move between panes and rows.

use gpui::*;

use glauca_core::engine::EngineCommand;
use glauca_core::types::*;

use super::*;

impl GlaucaApp {
    /// Mirror of the TUI run_app startup: prime unread counts for every root
    /// query, load the initially selected entry, and enqueue the rest for
    /// background refresh.
    pub(crate) fn prime(&mut self) {
        let root_ids: Vec<i64> = self
            .entries
            .iter()
            .filter_map(|e| match e {
                LeftPaneEntry::Query(q) => Some(q.id),
                LeftPaneEntry::FilterStream(_) => None,
            })
            .collect();
        for id in &root_ids {
            self.send(EngineCommand::LoadCached { query_id: *id });
        }

        let initially_synced_id = if self.entries.is_empty() {
            None
        } else {
            self.select_current_entry(false)
        };

        self.send(EngineCommand::EnqueueStale {
            skip_query_id: initially_synced_id,
        });
    }

    /// Send a command to the engine. Errors (channel closed/full) are ignored,
    /// matching the engine's own fire-and-forget semantics.
    pub(crate) fn send(&self, cmd: EngineCommand) {
        let _ = self.cmd_tx.try_send(cmd);
    }

    pub(crate) fn selected_root_query_id(&self) -> Option<i64> {
        self.entries
            .get(self.entry_cursor)
            .map(|e| e.root_query_id())
    }

    /// Display name of the selected left-pane entry (query label or stream name),
    /// shown in the center pane header.
    pub(crate) fn selected_entry_label(&self) -> Option<String> {
        self.entries.get(self.entry_cursor).map(|e| match e {
            LeftPaneEntry::Query(q) => q.label.clone(),
            LeftPaneEntry::FilterStream(fs) => fs.name.clone(),
        })
    }

    /// Commit a selection (click / Enter): load cached items, mark the entry
    /// viewed, and sync. Clears the current item view first.
    pub(crate) fn select_index(&mut self, index: usize) {
        if index >= self.entries.len() {
            return;
        }
        self.entry_cursor = index;
        self.items.clear();
        self.item_cursor = 0;
        self.reset_detail_scroll();
        self.clear_pending();
        self.recompute_filtered();
        self.select_current_entry(true);
    }

    /// Preview an entry (j/k cursor move): load cached items only — no sync and no
    /// mark-viewed, so scrolling through the list neither hits the network nor
    /// clears unread badges. Committing (Enter/click) does that via `select_index`.
    pub(crate) fn preview_entry(&mut self, index: usize) {
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let root_id = entry.root_query_id();
        let stream_filter = entry.stream_filter().map(|s| s.to_string());
        self.entry_cursor = index;
        self.items.clear();
        self.item_cursor = 0;
        self.reset_detail_scroll();
        self.stream_filter = stream_filter;
        self.clear_pending();
        self.recompute_filtered();
        self.send(EngineCommand::LoadCached { query_id: root_id });
    }

    /// Scroll the detail body back to the top. Called whenever the shown item
    /// changes (cursor move / entry switch / re-filter), mirroring the TUI's
    /// `detail_scroll = 0` reset.
    pub(crate) fn reset_detail_scroll(&self) {
        self.detail_scroll.set_offset(point(px(0.), px(0.)));
    }

    pub(crate) fn on_move_down(
        &mut self,
        _: &MoveDown,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.focus {
            Focus::QueryList => {
                if self.entry_cursor + 1 < self.entries.len() {
                    self.preview_entry(self.entry_cursor + 1);
                    cx.notify();
                }
            }
            Focus::ItemList => {
                let max = self.filtered_len().saturating_sub(1);
                if self.item_cursor < max {
                    let t = std::time::Instant::now();
                    self.item_cursor += 1;
                    self.items_list.scroll_to_reveal_item(self.item_cursor);
                    self.reset_detail_scroll();
                    self.mark_current_item_read(cx);
                    cx.notify();
                    tracing::debug!(
                        handler_us = t.elapsed().as_micros() as u64,
                        cursor = self.item_cursor,
                        "item move down"
                    );
                }
            }
            Focus::ItemDetail => {
                scroll_vertically(&self.detail_scroll, DETAIL_SCROLL_STEP);
                cx.notify();
            }
        }
    }

    pub(crate) fn on_move_up(&mut self, _: &MoveUp, _window: &mut Window, cx: &mut Context<Self>) {
        match self.focus {
            Focus::QueryList => {
                if self.entry_cursor > 0 {
                    self.preview_entry(self.entry_cursor - 1);
                    cx.notify();
                }
            }
            Focus::ItemList => {
                if self.item_cursor > 0 {
                    let t = std::time::Instant::now();
                    self.item_cursor -= 1;
                    self.items_list.scroll_to_reveal_item(self.item_cursor);
                    self.reset_detail_scroll();
                    self.mark_current_item_read(cx);
                    cx.notify();
                    tracing::debug!(
                        handler_us = t.elapsed().as_micros() as u64,
                        cursor = self.item_cursor,
                        "item move up"
                    );
                }
            }
            Focus::ItemDetail => {
                scroll_vertically(&self.detail_scroll, -DETAIL_SCROLL_STEP);
                cx.notify();
            }
        }
    }

    /// `h` cycles focus left: ItemDetail → ItemList → QueryList (clamped).
    pub(crate) fn on_focus_left(
        &mut self,
        _: &FocusLeft,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus = match self.focus {
            Focus::ItemDetail => Focus::ItemList,
            Focus::ItemList | Focus::QueryList => Focus::QueryList,
        };
        cx.notify();
    }

    /// `l` cycles focus right: QueryList → ItemList → ItemDetail (clamped).
    pub(crate) fn on_focus_right(
        &mut self,
        _: &FocusRight,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus = match self.focus {
            Focus::QueryList => Focus::ItemList,
            Focus::ItemList | Focus::ItemDetail => Focus::ItemDetail,
        };
        cx.notify();
    }

    pub(crate) fn on_activate(
        &mut self,
        _: &Activate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.focus {
            // Commit the previewed entry (sync + mark viewed).
            Focus::QueryList => {
                self.select_index(self.entry_cursor);
                cx.notify();
            }
            // ItemList / ItemDetail → action menu on the selected item, anchored
            // near the last pointer position (same PopupMenu as right-click).
            Focus::ItemList | Focus::ItemDetail => {
                if let Some(item) = self.selected_item() {
                    self.open_menu(
                        self.last_pointer,
                        MenuKind::Item(Box::new(item)),
                        window,
                        cx,
                    );
                }
            }
        }
    }

    pub(crate) fn on_focus_filter(
        &mut self,
        _: &FocusFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.filter_input.focus_handle(cx).focus(window, cx);
    }

    pub(crate) fn on_cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        // Esc closes the comments overlay first if it is open.
        if self.comments_open {
            self.close_comments(window, cx);
            return;
        }
        // Otherwise return focus to the root (leaves the filter box if focused).
        self.focus_handle.focus(window, cx);
        cx.notify();
    }
}
