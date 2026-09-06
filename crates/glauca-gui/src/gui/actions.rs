//! Item actions: open in browser, copy URL, run a custom action, and the
//! manual refresh of the current list/item.

use glauca_core::actions::CustomAction;
use glauca_core::engine::EngineCommand;

use super::*;

impl GlaucaApp {
    /// The item under the cursor in the (filtered) item list, if any.
    pub(crate) fn selected_item(&self) -> Option<ItemEntry> {
        self.filtered
            .get(self.item_cursor)
            .and_then(|&i| self.items.get(i))
            .cloned()
    }

    /// `o` — open the selected item in the browser, only from the item list. Also
    /// available via the action menu.
    pub(crate) fn on_open_in_browser(
        &mut self,
        _: &OpenInBrowser,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        if self.focus == Focus::QueryList {
            return;
        }
        if let Some(item) = self.selected_item() {
            self.send(EngineCommand::OpenBrowser {
                item: Box::new(item),
            });
        }
    }

    /// `y` — copy the selected item's URL to the clipboard. Also available via
    /// the action menu.
    pub(crate) fn on_copy_url(
        &mut self,
        _: &CopyUrl,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.focus == Focus::QueryList {
            return;
        }
        if let Some(item) = self.selected_item() {
            cx.write_to_clipboard(ClipboardItem::new_string(item.url));
        }
    }

    /// `x` — open the custom-action picker for the selected item, anchored near the last
    /// pointer position. No-op with a status hint when none applies to the item's kind.
    pub(crate) fn on_run_custom_action(
        &mut self,
        _: &RunCustomAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.focus == Focus::QueryList {
            return;
        }
        let Some(item) = self.selected_item() else {
            return;
        };
        let actions = self.actions_for_item(&item);
        if actions.is_empty() {
            self.status = Some("No custom actions for this item".into());
            cx.notify();
            return;
        }
        self.open_menu(
            self.last_pointer,
            MenuKind::CustomActions {
                item: Box::new(item),
                actions,
            },
            window,
            cx,
        );
    }

    /// Custom actions from `actions.toml` applicable to `item`'s kind, owned.
    /// Read here rather than inside `populate_menu` (which runs while the app
    /// entity is leased and must not `read` it).
    pub(crate) fn actions_for_item(&self, item: &ItemEntry) -> Vec<CustomAction> {
        self.custom_actions
            .for_kind(&item.kind)
            .into_iter()
            .cloned()
            .collect()
    }

    /// The query string of the root query with `root_id` — needed to re-sync the list
    /// backing a filter stream, which has no query string of its own.
    pub(crate) fn root_query_str_for(&self, root_id: i64) -> Option<String> {
        self.entries.iter().find_map(|e| match e {
            LeftPaneEntry::Query(q) if q.id == root_id => Some(q.query_str.clone()),
            _ => None,
        })
    }

    /// `r` — context-sensitive refresh: re-sync the selected list when the left
    /// pane is focused, otherwise re-fetch just the selected item.
    pub(crate) fn on_refresh(&mut self, _: &Refresh, _window: &mut Window, cx: &mut Context<Self>) {
        if self.focus == Focus::QueryList {
            self.refresh_selected_list();
        } else {
            self.refresh_selected_item();
        }
        cx.notify();
    }

    /// Re-sync the list for the selected entry (its root query) in place, keeping
    /// the current selection.
    pub(crate) fn refresh_selected_list(&mut self) {
        let Some(root_id) = self
            .entries
            .get(self.entry_cursor)
            .map(|e| e.root_query_id())
        else {
            return;
        };
        let Some(query_str) = self.root_query_str_for(root_id) else {
            self.status = Some("Nothing to refresh".into());
            return;
        };
        self.send(EngineCommand::Sync {
            query_id: root_id,
            query_str,
        });
        self.syncing = true;
    }

    /// Re-fetch just the selected item from GitHub into its query's cache.
    pub(crate) fn refresh_selected_item(&mut self) {
        let Some(item) = self.selected_item() else {
            return;
        };
        let Some(query_id) = self.selected_root_query_id() else {
            return;
        };
        let number = item.number;
        self.send(EngineCommand::RefreshItem {
            query_id,
            repo_owner: item.repo_owner,
            repo_name: item.repo_name,
            number,
        });
        self.status = Some(format!("Refreshing #{number}…"));
    }
}
