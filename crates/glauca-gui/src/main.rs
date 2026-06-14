//! glauca-gui — gpui front-end for glauca (phase B, MVP 閲覧先行).
//!
//! gpui owns the main-thread event loop and is not tokio-aware, so the async
//! engine runs on a separate multi-thread tokio runtime. The view periodically
//! drains `engine.try_recv()` and repaints; commands are sent from non-async
//! click handlers via a cloned `EngineCommand` sender (`engine.sender()`).
//!
//! B1: two panes. Left = the left-pane entries (root queries + indented filter
//! streams) as a clickable list with selection highlight and unread badges.
//! Center = the cached item list for the selected entry, with `NEW` badges and
//! scrolling. Selecting an entry mirrors the TUI `select_current_entry` flow:
//! load cached items, mark the entry viewed, and (for root queries) sync.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use glauca_core::engine::{AppMessage, Engine, EngineCommand, EngineInit};
use glauca_core::filter::FilterQuery;
use glauca_core::logic::{
    compute_unread_counts, expand_me, group_range, is_item_new_since, move_group_down,
};
use glauca_core::types::{CommentEntry, ItemAction, ItemEntry, LeftPaneEntry, MergeStrategy};
use glauca_core::{db, github};
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::Button;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use gpui_component::text::markdown;
use gpui_component::{h_flex, v_flex, ActiveTheme, Root, StyledExt, WindowExt};
use smol::Timer;
use tokio::sync::mpsc::Sender;

/// How often the GUI drains engine messages and repaints.
const DRAIN_INTERVAL: Duration = Duration::from_millis(50);

/// Idle delay before a filter keystroke triggers a re-filter, so typing fast in a
/// large list doesn't recompute on every character.
const FILTER_DEBOUNCE: Duration = Duration::from_millis(150);

/// Key-binding context for the root view. The gpui-component `Input` uses its own
/// `"Input"` context, so single-letter bindings scoped here never fire while the
/// user is typing in the filter box or a dialog text field.
const GLAUCA_CONTEXT: &str = "Glauca";

/// Predicate for navigation/edit keys: active under the root context but disabled
/// whenever an `Input` is in the focus path (so letters reach the text box), the
/// comments overlay is focused (its single-key controls take over), or a
/// `PopupMenu` (right-click / Enter action menu) is open (so its keys don't leak to
/// the underlying list). The `!` terms are matched against the full focus chain.
const NAV_CONTEXT: &str = "Glauca && !Input && !GlaucaComments && !PopupMenu";

/// Key-binding context for the comments overlay. While its panel is focused, the
/// overlay's single-key controls (j/k/g/G/s/h/q) fire here instead of the nav keys.
const COMMENTS_CONTEXT: &str = "GlaucaComments";

/// Pixels scrolled per j/k keypress in the detail pane and comments overlay.
const DETAIL_SCROLL_STEP: f32 = 48.0;

/// Scroll a tracked `overflow_y_scroll` container by `delta` pixels (positive =
/// down). gpui's scroll offset goes negative downward, clamped to the content.
fn scroll_vertically(handle: &ScrollHandle, delta: f32) {
    let mut off = handle.offset();
    off.y -= px(delta);
    let min_y = -handle.max_offset().y;
    if off.y < min_y {
        off.y = min_y;
    }
    if off.y > px(0.) {
        off.y = px(0.);
    }
    handle.set_offset(off);
}

gpui::actions!(
    glauca,
    [
        MoveDown,
        MoveUp,
        FocusLeft,
        FocusRight,
        Activate,
        FocusFilter,
        Cancel,
        NewQuery,
        NewFilterStream,
        EditEntry,
        DeleteEntry,
        ReorderDown,
        ReorderUp,
        Quit,
        OpenInBrowser,
        OpenComments,
        CommentsScrollDown,
        CommentsScrollUp,
        CommentsTop,
        CommentsBottom,
        CommentsToggleSort,
        CommentsToggleHidden,
        CommentsClose,
    ]
);

/// Which pane single-letter navigation keys act on. `h`/`l` cycle through the
/// three panes; in `ItemDetail` j/k scroll the detail body instead of moving the
/// item cursor (the detail pane mirrors the item cursor for its contents).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    QueryList,
    ItemList,
    ItemDetail,
}

/// What a context/action menu operates on (see `open_menu` / `populate_menu`).
enum MenuKind {
    /// Item-pane actions (open / comment / view comments / approve / merge).
    Item(ItemEntry),
    /// Left-pane entry actions for the entry at `index`.
    Entry { index: usize, is_query: bool },
    /// Empty area of the left pane: only "New query".
    NewQueryOnly,
}

struct GlaucaApp {
    engine: Engine,
    /// Cloneable command sender, used from non-async click handlers.
    cmd_tx: Sender<EngineCommand>,

    entries: Vec<LeftPaneEntry>,
    entry_cursor: usize,
    current_user: Option<String>,

    items: Vec<ItemEntry>,
    /// Indices into `items` passing the stream + inline filter. Cached so render
    /// (and the virtualized list) never re-scan all items per frame/keystroke;
    /// rebuilt by `recompute_filtered` only when items/filter/stream_filter change.
    filtered: Vec<usize>,
    /// Index into `filtered` of the row shown in the detail pane.
    item_cursor: usize,
    /// Inline filter text (mirrors the `filter_input` value; drives `filter_items`).
    filter: String,
    unread_counts: HashMap<(bool, i64), usize>,
    /// Filter stream filter applied to the item list (None for root queries).
    stream_filter: Option<String>,
    /// `last_viewed_at` of the selected entry at selection time; drives `is_new`.
    active_entry_last_viewed_at: Option<String>,

    /// Whether a manual GitHub sync is in progress for the selected query.
    syncing: bool,
    /// Number of pending background auto-refresh jobs (queued + in-progress).
    bg_sync_pending: usize,
    status: Option<String>,

    left_scroll: ScrollHandle,
    detail_scroll: ScrollHandle,
    /// Root focus handle — grabbed on startup so single-letter keys work; the
    /// filter Input takes focus on `/` and returns it on Esc.
    focus_handle: FocusHandle,
    /// Which pane j/k act on.
    focus: Focus,

    /// Comments overlay (View comments). Rendered as a self-managed overlay rather
    /// than a gpui dialog so async `CommentsLoaded` can repaint it via `cx.notify()`.
    comments_open: bool,
    comments: Vec<CommentEntry>,
    comments_loading: bool,
    comments_scroll: ScrollHandle,
    /// Newest-first when true (mirrors the TUI's `s` toggle; defaults to oldest-first).
    comments_sort_desc: bool,
    /// Show minimized/hidden comments expanded (TUI's `h` toggle).
    comments_show_hidden: bool,
    /// Header line of the overlay (`#<n> <title>` of the item it was opened for).
    comments_title: SharedString,
    /// Focus handle for the overlay panel; keys are scoped to `COMMENTS_CONTEXT`.
    comments_focus_handle: FocusHandle,

    /// Open right-click / Enter action menu (a self-managed anchored PopupMenu).
    menu: Option<Entity<PopupMenu>>,
    /// Screen position the menu is anchored at.
    menu_pos: Point<Pixels>,
    /// Last pointer position (updated on mouse move, no repaint); used to anchor the
    /// menu when opened via the keyboard (Enter), so it appears near the cursor.
    last_pointer: Point<Pixels>,

    /// Inline filter input. Its `Change` events update `filter` (see `new`).
    filter_input: Entity<InputState>,
    /// Pending debounced re-filter task; replacing it cancels the previous one.
    filter_task: Option<Task<()>>,
    /// Keeps the `filter_input` subscription alive for the view's lifetime.
    _subscriptions: Vec<Subscription>,
}

impl GlaucaApp {
    fn new(engine: Engine, init: EngineInit, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Periodically drain engine messages and repaint while the window lives.
        cx.spawn(async move |this, cx| {
            loop {
                Timer::after(DRAIN_INTERVAL).await;
                let result = this.update(cx, |this, cx| {
                    let mut changed = false;
                    while let Some(msg) = this.engine.try_recv() {
                        this.apply(msg, cx);
                        changed = true;
                    }
                    if changed {
                        cx.notify();
                    }
                });
                if result.is_err() {
                    // Entity gone (window closed) — stop the loop.
                    break;
                }
            }
        })
        .detach();

        let cmd_tx = engine.sender();
        let filter_input = cx.new(|cx| InputState::new(window, cx).placeholder("filter…"));
        // Mirror the input value into `filter` (and reset the item cursor) on every
        // change so `filter_items` re-runs and the detail pane stays in range.
        let subscription = cx.subscribe_in(
            &filter_input,
            window,
            |this, input, ev: &InputEvent, _window, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.filter = input.read(cx).value().to_string();
                    // Debounce: re-filter only after typing pauses. Replacing the
                    // task drops (cancels) any still-pending one.
                    this.filter_task = Some(cx.spawn(async move |this, cx| {
                        Timer::after(FILTER_DEBOUNCE).await;
                        let _ = this.update(cx, |this, cx| {
                            this.item_cursor = 0;
                            this.recompute_filtered();
                            cx.notify();
                        });
                    }));
                }
            },
        );
        let EngineInit {
            entries,
            current_user,
        } = init;
        let mut app = Self {
            engine,
            cmd_tx,
            entries,
            entry_cursor: 0,
            current_user,
            items: Vec::new(),
            filtered: Vec::new(),
            item_cursor: 0,
            filter: String::new(),
            unread_counts: HashMap::new(),
            stream_filter: None,
            active_entry_last_viewed_at: None,
            syncing: false,
            bg_sync_pending: 0,
            status: None,
            left_scroll: ScrollHandle::new(),
            detail_scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            focus: Focus::QueryList,
            comments_open: false,
            comments: Vec::new(),
            comments_loading: false,
            comments_scroll: ScrollHandle::new(),
            comments_sort_desc: false,
            comments_show_hidden: false,
            comments_title: SharedString::default(),
            comments_focus_handle: cx.focus_handle(),
            menu: None,
            menu_pos: point(px(0.), px(0.)),
            last_pointer: point(px(0.), px(0.)),
            filter_input,
            filter_task: None,
            _subscriptions: vec![subscription],
        };
        app.prime();
        // Grab keyboard focus so single-letter navigation works without a click.
        app.focus_handle.focus(window, cx);
        app
    }

    /// Mirror of the TUI run_app startup: prime unread counts for every root
    /// query, load the initially selected entry, and enqueue the rest for
    /// background refresh.
    fn prime(&mut self) {
        let root_ids: Vec<i64> = self
            .entries
            .iter()
            .filter_map(|e| match e {
                LeftPaneEntry::Query(q) => Some(q.id),
                LeftPaneEntry::FilterStream(_) => None,
            })
            .collect();
        for id in &root_ids {
            self.send(EngineCommand::LoadCached {
                query_id: *id,
                highlight_since: None,
            });
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
    fn send(&self, cmd: EngineCommand) {
        let _ = self.cmd_tx.try_send(cmd);
    }

    fn selected_root_query_id(&self) -> Option<i64> {
        self.entries.get(self.entry_cursor).map(|e| e.root_query_id())
    }

    /// Commit a selection (click / Enter): load cached items, mark the entry
    /// viewed, and sync. Clears the current item view first.
    fn select_index(&mut self, index: usize) {
        if index >= self.entries.len() {
            return;
        }
        self.entry_cursor = index;
        self.items.clear();
        self.item_cursor = 0;
        self.recompute_filtered();
        self.select_current_entry(true);
    }

    /// Preview an entry (j/k cursor move): load cached items only — no sync and no
    /// mark-viewed, so scrolling through the list neither hits the network nor
    /// clears unread badges. Committing (Enter/click) does that via `select_index`.
    fn preview_entry(&mut self, index: usize) {
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let root_id = entry.root_query_id();
        let highlight_since = entry.last_viewed_at().map(str::to_string);
        let stream_filter = entry.stream_filter().map(|s| s.to_string());
        self.entry_cursor = index;
        self.items.clear();
        self.item_cursor = 0;
        self.stream_filter = stream_filter;
        self.active_entry_last_viewed_at = highlight_since.clone();
        self.recompute_filtered();
        self.send(EngineCommand::LoadCached {
            query_id: root_id,
            highlight_since,
        });
    }

    // ── Keyboard action handlers ──────────────────────────────────────────────

    fn on_move_down(&mut self, _: &MoveDown, _window: &mut Window, cx: &mut Context<Self>) {
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
                    self.item_cursor += 1;
                    self.mark_current_item_read(cx);
                }
            }
            Focus::ItemDetail => {
                scroll_vertically(&self.detail_scroll, DETAIL_SCROLL_STEP);
                cx.notify();
            }
        }
    }

    fn on_move_up(&mut self, _: &MoveUp, _window: &mut Window, cx: &mut Context<Self>) {
        match self.focus {
            Focus::QueryList => {
                if self.entry_cursor > 0 {
                    self.preview_entry(self.entry_cursor - 1);
                    cx.notify();
                }
            }
            Focus::ItemList => {
                if self.item_cursor > 0 {
                    self.item_cursor -= 1;
                    self.mark_current_item_read(cx);
                }
            }
            Focus::ItemDetail => {
                scroll_vertically(&self.detail_scroll, -DETAIL_SCROLL_STEP);
                cx.notify();
            }
        }
    }

    /// `h` cycles focus left: ItemDetail → ItemList → QueryList (clamped).
    fn on_focus_left(&mut self, _: &FocusLeft, _window: &mut Window, cx: &mut Context<Self>) {
        self.focus = match self.focus {
            Focus::ItemDetail => Focus::ItemList,
            Focus::ItemList | Focus::QueryList => Focus::QueryList,
        };
        cx.notify();
    }

    /// `l` cycles focus right: QueryList → ItemList → ItemDetail (clamped).
    fn on_focus_right(&mut self, _: &FocusRight, _window: &mut Window, cx: &mut Context<Self>) {
        self.focus = match self.focus {
            Focus::QueryList => Focus::ItemList,
            Focus::ItemList | Focus::ItemDetail => Focus::ItemDetail,
        };
        cx.notify();
    }

    fn on_activate(&mut self, _: &Activate, window: &mut Window, cx: &mut Context<Self>) {
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
                    self.open_menu(self.last_pointer, MenuKind::Item(item), window, cx);
                }
            }
        }
    }

    fn on_focus_filter(&mut self, _: &FocusFilter, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_input.focus_handle(cx).focus(window, cx);
    }

    fn on_cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        // Esc closes the comments overlay first if it is open.
        if self.comments_open {
            self.close_comments(window, cx);
            return;
        }
        // Otherwise return focus to the root (leaves the filter box if focused).
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    /// The item under the cursor in the (filtered) item list, if any.
    fn selected_item(&self) -> Option<ItemEntry> {
        self.filtered
            .get(self.item_cursor)
            .and_then(|&i| self.items.get(i))
            .cloned()
    }

    /// `o` — open the selected item in the browser (only from the item list,
    /// mirroring the TUI). Also available via the action menu.
    fn on_open_in_browser(&mut self, _: &OpenInBrowser, _window: &mut Window, _cx: &mut Context<Self>) {
        if self.focus == Focus::QueryList {
            return;
        }
        if let Some(item) = self.selected_item() {
            self.send(EngineCommand::OpenBrowser { item });
        }
    }

    /// `c` — open the comments overlay for the selected item.
    fn on_open_comments(&mut self, _: &OpenComments, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus == Focus::QueryList {
            return;
        }
        if let Some(item) = self.selected_item() {
            self.open_comments(item, window, cx);
        }
    }

    /// Open the comments overlay for `item` and request its comments. Clearing
    /// `comments` + setting `comments_loading` first means a quick reopen never
    /// shows the previous item's comments.
    fn open_comments(&mut self, item: ItemEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.comments.clear();
        self.comments_loading = true;
        self.comments_open = true;
        self.comments_sort_desc = false;
        self.comments_show_hidden = false;
        self.comments_scroll.set_offset(point(px(0.), px(0.)));
        self.comments_title =
            SharedString::from(format!("Comments — #{} {}", item.number, item.title));
        self.send(EngineCommand::LoadComments {
            owner: item.repo_owner.clone(),
            repo: item.repo_name.clone(),
            number: item.number as u64,
        });
        self.comments_focus_handle.focus(window, cx);
        cx.notify();
    }

    /// Close the comments overlay and return focus to the root so nav keys work.
    fn close_comments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.comments_open = false;
        self.comments_loading = false;
        self.comments.clear();
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    // ── Comments overlay keys (scoped to COMMENTS_CONTEXT) ───────────────────────

    fn on_comments_scroll_down(
        &mut self,
        _: &CommentsScrollDown,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        scroll_vertically(&self.comments_scroll, DETAIL_SCROLL_STEP);
        cx.notify();
    }

    fn on_comments_scroll_up(
        &mut self,
        _: &CommentsScrollUp,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        scroll_vertically(&self.comments_scroll, -DETAIL_SCROLL_STEP);
        cx.notify();
    }

    fn on_comments_top(&mut self, _: &CommentsTop, _window: &mut Window, cx: &mut Context<Self>) {
        self.comments_scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    fn on_comments_bottom(
        &mut self,
        _: &CommentsBottom,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.comments_scroll.scroll_to_bottom();
        cx.notify();
    }

    fn on_comments_toggle_sort(
        &mut self,
        _: &CommentsToggleSort,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.comments_sort_desc = !self.comments_sort_desc;
        self.comments_scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    fn on_comments_toggle_hidden(
        &mut self,
        _: &CommentsToggleHidden,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.comments_show_hidden = !self.comments_show_hidden;
        self.comments_scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    fn on_comments_close(
        &mut self,
        _: &CommentsClose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_comments(window, cx);
    }

    fn on_quit(&mut self, _: &Quit, _window: &mut Window, cx: &mut Context<Self>) {
        cx.quit();
    }

    fn on_delete_entry(&mut self, _: &DeleteEntry, _window: &mut Window, _cx: &mut Context<Self>) {
        if self.focus != Focus::QueryList {
            return;
        }
        self.delete_entry_at(self.entry_cursor);
    }

    /// Delete the entry at `index` (query or filter stream). UI updates when the
    /// QueryDeleted/FilterStreamDeleted confirmation arrives.
    fn delete_entry_at(&mut self, index: usize) {
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

    fn on_reorder_down(&mut self, _: &ReorderDown, _window: &mut Window, _cx: &mut Context<Self>) {
        self.reorder(true);
    }

    fn on_reorder_up(&mut self, _: &ReorderUp, _window: &mut Window, _cx: &mut Context<Self>) {
        self.reorder(false);
    }

    /// Move the selected entry up/down within its group. Sends a swap command; the
    /// entries vec is reordered when the *Swapped confirmation arrives (mirrors the
    /// TUI's J/K handling).
    fn reorder(&mut self, down: bool) {
        if self.focus != Focus::QueryList {
            return;
        }
        self.reorder_entry(self.entry_cursor, down);
    }

    /// Move the entry at `cursor` up/down within its group (index-based; no focus
    /// guard so right-click can target any row).
    fn reorder_entry(&mut self, cursor: usize, down: bool) {
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

    fn filtered_len(&self) -> usize {
        self.filtered.len()
    }

    /// Rebuild the `filtered` index cache from `items` + stream/inline filters.
    /// Mirrors `glauca_core::logic::filter_items` but yields indices so render can
    /// reuse them without re-scanning every frame. Call after any change to
    /// `items`, `filter`, or `stream_filter`.
    fn recompute_filtered(&mut self) {
        let stream_q = self
            .stream_filter
            .as_deref()
            .map(|s| FilterQuery::parse(&expand_me(self.current_user.as_deref(), s)));
        let inline_q = FilterQuery::parse(&expand_me(self.current_user.as_deref(), &self.filter));
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                stream_q.as_ref().map_or(true, |q| q.matches(item))
                    && (inline_q.is_empty() || inline_q.matches(item))
            })
            .map(|(ix, _)| ix)
            .collect();
        if self.item_cursor >= self.filtered.len() {
            self.item_cursor = self.filtered.len().saturating_sub(1);
        }
    }

    // ── Entry add / edit dialogs ───────────────────────────────────────────────

    fn on_new_query(&mut self, _: &NewQuery, window: &mut Window, cx: &mut Context<Self>) {
        self.new_query(window, cx);
    }

    /// Open the "add query" form.
    fn new_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_query_form(None, String::new(), String::new(), window, cx);
    }

    fn on_new_filter_stream(
        &mut self,
        _: &NewFilterStream,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_filter_stream_under(self.entry_cursor, window, cx);
    }

    /// Open the "add filter stream" form parented to the entry at `index`'s root query.
    fn new_filter_stream_under(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let parent_id = entry.root_query_id();
        let kind = entry.kind().to_string();
        self.open_filter_stream_form(None, parent_id, kind, String::new(), String::new(), window, cx);
    }

    fn on_edit_entry(&mut self, _: &EditEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_entry_at(self.entry_cursor, window, cx);
    }

    /// Open the edit form for the entry at `index` (query or filter stream).
    fn edit_entry_at(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(LeftPaneEntry::Query(q)) = self.entries.get(index) {
            let (id, name, query) = (q.id, q.label.clone(), q.query_str.clone());
            self.open_query_form(Some(id), name, query, window, cx);
        } else if let Some(LeftPaneEntry::FilterStream(fs)) = self.entries.get(index) {
            let (id, parent, kind, name, filter) = (
                fs.id,
                fs.parent_id,
                fs.kind.clone(),
                fs.name.clone(),
                fs.filter.clone(),
            );
            self.open_filter_stream_form(Some(id), parent, kind, name, filter, window, cx);
        }
    }

    /// Add (`edit=None`) or edit (`edit=Some(id)`) a root query via a 2-field dialog.
    fn open_query_form(
        &mut self,
        edit: Option<i64>,
        init_name: String,
        init_query: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("display name (optional)")
                .default_value(init_name)
        });
        let query = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("GitHub search query (e.g. repo:owner/name is:pr is:open)")
                .default_value(init_query)
        });
        let this = cx.weak_entity();
        let title = if edit.is_some() { "Edit query" } else { "Add query" };
        window.open_dialog(cx, move |dlg, _w, _cx| {
            let (name_c, query_c) = (name.clone(), query.clone());
            let (name_ok, query_ok) = (name.clone(), query.clone());
            let this = this.clone();
            dlg.title(title)
                .w(px(520.))
                .content(move |content, _w, _cx| {
                    content
                        .gap_3()
                        .child(Input::new(&name_c))
                        .child(Input::new(&query_c))
                })
                .on_ok(move |_, _w, cx| {
                    let n = name_ok.read(cx).value().to_string();
                    let q = query_ok.read(cx).value().to_string();
                    let (n, q) = (n.trim().to_string(), q.trim().to_string());
                    if !q.is_empty() {
                        let name = if n.is_empty() { None } else { Some(n) };
                        if let Some(app) = this.upgrade() {
                            app.update(cx, |app, _| match edit {
                                Some(id) => app.send(EngineCommand::EditQuery {
                                    id,
                                    name,
                                    query: q,
                                }),
                                None => app.send(EngineCommand::AddQuery { name, query: q }),
                            });
                        }
                    }
                    true
                })
        });
    }

    /// Add (`edit=None`) or edit (`edit=Some(id)`) a filter stream via a 2-field dialog.
    fn open_filter_stream_form(
        &mut self,
        edit: Option<i64>,
        parent_id: i64,
        kind: String,
        init_name: String,
        init_filter: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("display name")
                .default_value(init_name)
        });
        let filter = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("filter (e.g. state:open label:bug)")
                .default_value(init_filter)
        });
        let this = cx.weak_entity();
        let title = if edit.is_some() {
            "Edit filter stream"
        } else {
            "Add filter stream"
        };
        window.open_dialog(cx, move |dlg, _w, _cx| {
            let (name_c, filter_c) = (name.clone(), filter.clone());
            let (name_ok, filter_ok) = (name.clone(), filter.clone());
            let this = this.clone();
            let kind = kind.clone();
            dlg.title(title)
                .w(px(520.))
                .content(move |content, _w, _cx| {
                    content
                        .gap_3()
                        .child(Input::new(&name_c))
                        .child(Input::new(&filter_c))
                })
                .on_ok(move |_, _w, cx| {
                    let n = name_ok.read(cx).value().to_string();
                    let f = filter_ok.read(cx).value().to_string();
                    let (n, f) = (n.trim().to_string(), f.trim().to_string());
                    if !n.is_empty() && !f.is_empty() {
                        let kind = kind.clone();
                        if let Some(app) = this.upgrade() {
                            app.update(cx, |app, _| match edit {
                                Some(id) => app.send(EngineCommand::EditFilterStream {
                                    id,
                                    name: n,
                                    filter: f,
                                }),
                                None => app.send(EngineCommand::AddFilterStream {
                                    parent_id,
                                    kind,
                                    name: n,
                                    filter: f,
                                }),
                            });
                        }
                    }
                    true
                })
        });
    }

    /// Issue the engine commands to (re)load the currently selected entry: load
    /// cached items, mark it viewed, and—for root queries—sync. Returns the root
    /// query id when a query (not a filter stream) was selected, so the caller
    /// can skip it from the background-refresh sweep.
    fn select_current_entry(&mut self, always_sync: bool) -> Option<i64> {
        let entry = self.entries.get(self.entry_cursor)?.clone();
        // Selecting a query does NOT mark it viewed: the unread badge is kept and
        // reflects the engine's stored `last_viewed_at` (counts only change as sync
        // brings newer items). Previously this optimistically set last_viewed_at=now
        // and zeroed the count, which cleared the badge on every open and left it
        // diverged from the recomputed value until the next background sync.
        let highlight_since = entry.last_viewed_at().map(str::to_string);
        self.stream_filter = entry.stream_filter().map(|s| s.to_string());
        self.active_entry_last_viewed_at = highlight_since.clone();

        let root_id = entry.root_query_id();
        self.send(EngineCommand::LoadCached {
            query_id: root_id,
            highlight_since: highlight_since.clone(),
        });
        if entry.is_filter_stream() {
            return None;
        }

        let query_str = entry.root_query_str().unwrap_or_default().to_string();
        if always_sync {
            self.send(EngineCommand::Sync {
                query_id: root_id,
                query_str,
                highlight_since,
            });
            self.syncing = true;
        } else {
            self.send(EngineCommand::SyncIfStale {
                query_id: root_id,
                query_str,
                highlight_since,
            });
        }
        Some(root_id)
    }

    // ── Item actions ───────────────────────────────────────────────────────────

    /// Open a `PopupMenu` (right-click / Enter action menu) anchored at `pos`. The
    /// menu is a self-managed anchored overlay (see `render`); it focuses itself and
    /// emits `DismissEvent` on selection/Esc/outside-click, which clears `self.menu`.
    fn open_menu(
        &mut self,
        pos: Point<Pixels>,
        kind: MenuKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let app = cx.entity();
        let menu = PopupMenu::build(window, cx, move |menu, _w, _cx| populate_menu(menu, &app, kind));
        cx.subscribe(&menu, |this, _menu, _e: &DismissEvent, cx| {
            this.menu = None;
            cx.notify();
        })
        .detach();
        menu.focus_handle(cx).focus(window, cx);
        self.menu = Some(menu);
        self.menu_pos = pos;
        cx.notify();
    }

    fn dispatch_action(
        &mut self,
        action: ItemAction,
        item: ItemEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            ItemAction::OpenBrowser => self.send(EngineCommand::OpenBrowser { item }),
            ItemAction::Comment => self.open_comment_dialog(item, window, cx),
            ItemAction::ApprovePR => self.open_approve_dialog(item, window, cx),
            ItemAction::MergePR => self.open_merge_dialog(item, window, cx),
            ItemAction::ViewComments => self.open_comments(item, window, cx),
        }
    }

    fn open_comment_dialog(&mut self, item: ItemEntry, window: &mut Window, cx: &mut Context<Self>) {
        let body = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(3, 12)
                .placeholder("Comment body")
        });
        let this = cx.weak_entity();
        window.open_dialog(cx, move |dlg, _w, _cx| {
            let body_c = body.clone();
            let body_ok = body.clone();
            let this = this.clone();
            let item = item.clone();
            dlg.title("Comment").w(px(560.))
                .content(move |content, _w, _cx| content.child(Input::new(&body_c).h(px(220.))))
                .on_ok(move |_, _w, cx| {
                    let b = body_ok.read(cx).value().to_string();
                    let b = b.trim().to_string();
                    if !b.is_empty() {
                        if let Some(app) = this.upgrade() {
                            let item = item.clone();
                            app.update(cx, |app, _| {
                                app.send(EngineCommand::Comment {
                                    url: item.url.clone(),
                                    kind: item.kind.clone(),
                                    body: b,
                                })
                            });
                        }
                    }
                    true
                })
        });
    }

    fn open_approve_dialog(&mut self, item: ItemEntry, window: &mut Window, cx: &mut Context<Self>) {
        let body = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(3, 12)
                .placeholder("Optional review comment")
        });
        let this = cx.weak_entity();
        window.open_dialog(cx, move |dlg, _w, _cx| {
            let body_c = body.clone();
            let body_ok = body.clone();
            let this = this.clone();
            let item = item.clone();
            dlg.title("Approve PR").w(px(560.))
                .content(move |content, _w, _cx| content.child(Input::new(&body_c).h(px(180.))))
                .on_ok(move |_, _w, cx| {
                    let b = body_ok.read(cx).value().to_string();
                    let b = b.trim().to_string();
                    let body = if b.is_empty() { None } else { Some(b) };
                    if let Some(app) = this.upgrade() {
                        let item = item.clone();
                        app.update(cx, |app, _| {
                            app.send(EngineCommand::Approve {
                                url: item.url.clone(),
                                body,
                            })
                        });
                    }
                    true
                })
        });
    }

    fn open_merge_dialog(&mut self, item: ItemEntry, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        window.open_dialog(cx, move |dlg, _w, _cx| {
            let this = this.clone();
            let item = item.clone();
            dlg.title("Merge strategy").w(px(320.)).content(move |content, _w, _cx| {
                let mut col = content.gap_2();
                for (ix, strat) in MergeStrategy::all().into_iter().enumerate() {
                    let label = strat.label().to_string();
                    let item = item.clone();
                    let this = this.clone();
                    col = col.child(
                        Button::new(("merge", ix))
                            .label(label)
                            .on_click(move |_, window, cx| {
                                let item = item.clone();
                                let strat = strat.clone();
                                let this = this.clone();
                                window.close_dialog(cx);
                                if let Some(app) = this.upgrade() {
                                    app.update(cx, |app, _| {
                                        app.send(EngineCommand::Merge {
                                            url: item.url.clone(),
                                            strategy: strat,
                                        })
                                    });
                                }
                            }),
                    );
                }
                col
            })
        });
    }

    fn recompute_unread(&mut self, query_id: i64, items: &[ItemEntry]) {
        for (key, unread) in
            compute_unread_counts(&self.entries, query_id, items, self.current_user.as_deref())
        {
            self.unread_counts.insert(key, unread);
        }
    }

    /// Mark the currently-selected item read (it is shown in the detail pane):
    /// flip its in-memory `read` flag, recompute the current query's unread
    /// badges, and persist via the engine (fire-and-forget). No-op if there is no
    /// selection or the item is already read.
    fn mark_current_item_read(&mut self, cx: &mut Context<Self>) {
        let Some(&idx) = self.filtered.get(self.item_cursor) else {
            return;
        };
        match self.items.get(idx) {
            Some(item) if !item.read => {}
            _ => return,
        }
        self.items[idx].read = true;
        let (repo_owner, repo_name, number) = {
            let item = &self.items[idx];
            (item.repo_owner.clone(), item.repo_name.clone(), item.number)
        };
        if let Some(query_id) = self.selected_root_query_id() {
            // Recompute from the live items (compute → then insert, to avoid
            // borrowing self mutably while reading self.items/entries).
            let updates = compute_unread_counts(
                &self.entries,
                query_id,
                &self.items,
                self.current_user.as_deref(),
            );
            for (key, unread) in updates {
                self.unread_counts.insert(key, unread);
            }
            self.send(EngineCommand::MarkItemRead {
                query_id,
                repo_owner,
                repo_name,
                number,
            });
        }
        cx.notify();
    }

    /// Apply a single engine message to GUI state. Mirrors the TUI's `run_app`
    /// message handling (crates/glauca-cli/src/tui/mod.rs).
    fn apply(&mut self, msg: AppMessage, _cx: &mut Context<Self>) {
        // Only rebuild the filtered-index cache when items/filter/stream_filter
        // actually change. Background sync floods `apply` with messages that don't
        // touch the visible list (other queries' ItemsLoaded, Status, BgSync*,
        // EntryViewed, …); recomputing on each would re-scan all items (~thousands)
        // on the UI thread and make the app sluggish while sync runs. Selection and
        // filter edits recompute in their own handlers (`select_index`,
        // `preview_entry`, the filter debounce task).
        let mut needs_refilter = false;
        match msg {
            AppMessage::ItemsLoaded { query_id, mut items } => {
                self.recompute_unread(query_id, &items);
                if self.selected_root_query_id() == Some(query_id) {
                    let highlight_since = self.active_entry_last_viewed_at.clone();
                    for item in &mut items {
                        item.is_new =
                            is_item_new_since(&item.cached_at, highlight_since.as_deref());
                    }
                    self.items = items;
                    needs_refilter = true;
                }
            }
            AppMessage::EntryViewed { entry_id, viewed_at } => {
                if let Some(entry) = self.entries.iter_mut().find(|e| e.id() == entry_id) {
                    entry.set_last_viewed_at(Some(viewed_at));
                }
            }
            AppMessage::SyncStarted { .. } => self.syncing = true,
            AppMessage::SyncDone { count, .. } => {
                self.syncing = false;
                self.status = Some(format!("Synced {count} items"));
            }
            AppMessage::SyncError { error, .. } => {
                self.syncing = false;
                self.status = Some(format!("Sync error: {error}"));
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
            AppMessage::QueryUpdated { id, new_name, new_query } => {
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
                    let highlight_since = self.active_entry_last_viewed_at.clone();
                    self.send(EngineCommand::LoadCached {
                        query_id: id,
                        highlight_since: highlight_since.clone(),
                    });
                    self.send(EngineCommand::Sync {
                        query_id: id,
                        query_str: new_query,
                        highlight_since,
                    });
                    self.syncing = true;
                    needs_refilter = true;
                }
                self.status = Some("Query updated".into());
            }
            AppMessage::FilterStreamUpdated { id, new_name, new_filter } => {
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
                    let items = self.items.clone();
                    self.recompute_unread(root_id, &items);
                }
                self.status = Some("Filter stream updated".into());
            }
            AppMessage::QueryDeleted { query_id } => {
                self.entries.retain(|e| e.root_query_id() != query_id);
                if self.entry_cursor >= self.entries.len() {
                    self.entry_cursor = self.entries.len().saturating_sub(1);
                }
                self.filter.clear();
                if self.entries.is_empty() {
                    self.items.clear();
                    self.stream_filter = None;
                    needs_refilter = true;
                } else {
                    self.select_index(self.entry_cursor);
                }
            }
            AppMessage::FilterStreamDeleted { id } => {
                self.entries.retain(|e| e.id() != id);
                if self.entry_cursor >= self.entries.len() {
                    self.entry_cursor = self.entries.len().saturating_sub(1);
                }
                self.filter.clear();
                if self.entries.is_empty() {
                    self.items.clear();
                    self.stream_filter = None;
                    needs_refilter = true;
                } else {
                    self.select_index(self.entry_cursor);
                }
            }
            AppMessage::QueriesSwapped { upper_id, active_id, .. } => {
                if let Some(idx) = self.entries.iter().position(
                    |e| matches!(e, LeftPaneEntry::Query(q) if q.id == upper_id),
                ) {
                    move_group_down(&mut self.entries, idx);
                }
                if let Some(pos) = self.entries.iter().position(|e| e.id() == active_id) {
                    self.entry_cursor = pos;
                }
            }
            AppMessage::FilterStreamsSwapped { upper_id, lower_id, active_id } => {
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
            AppMessage::ActionError(e) => self.status = Some(format!("Error: {e}")),

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
            }
        }
        if needs_refilter {
            self.recompute_filtered();
        }
    }

    fn render_left(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut col = v_flex()
            .id("left-pane")
            .w(px(280.))
            .h_full()
            .flex_shrink_0()
            .overflow_y_scroll()
            .track_scroll(&self.left_scroll)
            .border_r_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar);

        for (i, entry) in self.entries.iter().enumerate() {
            let selected = i == self.entry_cursor;
            let is_stream = entry.is_filter_stream();
            let is_query = matches!(entry, LeftPaneEntry::Query(_));
            let label = match entry {
                LeftPaneEntry::Query(q) => q.label.clone(),
                LeftPaneEntry::FilterStream(fs) => fs.name.clone(),
            };
            let unread = self.unread_counts.get(&entry.unread_key()).copied().unwrap_or(0);

            let row = h_flex()
                .id(("entry", i))
                .w_full()
                .px_3()
                .py_1p5()
                .gap_2()
                .items_center()
                .cursor_pointer()
                .when(is_stream, |e| e.pl(px(28.)))
                .when(selected, |e| e.bg(cx.theme().list_active))
                .when(!selected, |e| e.hover(|e| e.bg(cx.theme().list_hover)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(cx.theme().sidebar_foreground)
                        .child(SharedString::from(label)),
                )
                .when(unread > 0, |e| {
                    e.child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(cx.theme().accent_foreground)
                            .bg(cx.theme().accent)
                            .px_1p5()
                            .rounded_full()
                            .child(SharedString::from(unread.to_string())),
                    )
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.focus = Focus::QueryList;
                    this.select_index(i);
                    cx.notify();
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                        this.focus = Focus::QueryList;
                        this.entry_cursor = i;
                        this.open_menu(
                            ev.position,
                            MenuKind::Entry { index: i, is_query },
                            window,
                            cx,
                        );
                    }),
                );

            col = col.child(row);
        }

        // Empty area below the entries: right-click → New query. Kept as its own
        // flex_1 element so a right-click on a row hits the row handler, not this.
        col.child(
            div()
                .id("left-empty")
                .flex_1()
                .min_h(px(24.))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                        this.open_menu(ev.position, MenuKind::NewQueryOnly, window, cx);
                    }),
                ),
        )
    }

    fn render_items(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // The filtered set is precomputed (`recompute_filtered`); rows are built
        // lazily per visible range by `uniform_list`. Re-scanning all items here
        // (or eagerly building every row) is what made filtering/large queries lag.
        let count = self.filtered.len();

        let container = v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .bg(cx.theme().background);

        if count == 0 {
            return container.child(
                div()
                    .p_4()
                    .text_color(cx.theme().muted_foreground)
                    .child("No items"),
            );
        }

        container.child(
            uniform_list(
                "items-list",
                count,
                cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                    // Inline-filter query, used to highlight matching title text.
                    let fq = FilterQuery::parse(&expand_me(
                        this.current_user.as_deref(),
                        &this.filter,
                    ));
                    let selected = this.item_cursor;
                    let mut rows = Vec::new();
                    for ix in range {
                        let Some(item) = this.filtered.get(ix).and_then(|&i| this.items.get(i))
                        else {
                            continue;
                        };
                        let mut meta = format!(
                            "{}/{}#{}  ·  {}  ·  @{}",
                            item.repo_owner,
                            item.repo_name,
                            item.number,
                            item.state,
                            item.author.as_deref().unwrap_or("ghost"),
                        );
                        if !item.labels.is_empty() {
                            meta.push_str("  ·  ");
                            meta.push_str(&item.labels.join(", "));
                        }
                        let is_new = item.is_new;
                        let title_el = highlight_title(&item.title, fq.highlight_ranges(&item.title), cx);

                        rows.push(
                            v_flex()
                                .id(ix)
                                .h(px(52.))
                                .w_full()
                                .px_4()
                                .justify_center()
                                .gap_0p5()
                                .border_b_1()
                                .border_color(cx.theme().border)
                                .cursor_pointer()
                                .when(ix == selected, |e| e.bg(cx.theme().list_active))
                                .when(ix != selected, |e| {
                                    e.hover(|e| e.bg(cx.theme().list_hover))
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.focus = Focus::ItemList;
                                    this.item_cursor = ix;
                                    this.mark_current_item_read(cx);
                                }))
                                .on_mouse_down(
                                    MouseButton::Right,
                                    cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                                        this.focus = Focus::ItemList;
                                        this.item_cursor = ix;
                                        if let Some(item) = this.selected_item() {
                                            this.open_menu(
                                                ev.position,
                                                MenuKind::Item(item),
                                                window,
                                                cx,
                                            );
                                        }
                                    }),
                                )
                                .child(
                                    h_flex()
                                        .w_full()
                                        .gap_2()
                                        .items_center()
                                        .when(is_new, |e| {
                                            e.child(
                                                div()
                                                    .flex_shrink_0()
                                                    .text_xs()
                                                    .font_bold()
                                                    .text_color(cx.theme().accent_foreground)
                                                    .bg(cx.theme().accent)
                                                    .px_1p5()
                                                    .rounded_md()
                                                    .child("NEW"),
                                            )
                                        })
                                        .child(title_el),
                                )
                                .child(
                                    div()
                                        .w_full()
                                        .truncate()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(SharedString::from(meta)),
                                ),
                        );
                    }
                    rows
                }),
            )
            .h_full(),
        )
    }

    fn render_detail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Highlight the left border when the detail pane has keyboard focus
        // (j/k scroll it); otherwise use the neutral divider color.
        let border = if self.focus == Focus::ItemDetail {
            cx.theme().primary
        } else {
            cx.theme().border
        };
        let container = v_flex()
            .id("detail-pane")
            .w(px(440.))
            .h_full()
            .flex_shrink_0()
            .overflow_y_scroll()
            .track_scroll(&self.detail_scroll)
            .border_l_1()
            .border_color(border)
            .bg(cx.theme().background)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    if let Some(item) = this.selected_item() {
                        this.open_menu(ev.position, MenuKind::Item(item), window, cx);
                    }
                }),
            );

        let Some(item) = self.filtered.get(self.item_cursor).and_then(|&i| self.items.get(i))
        else {
            return container.child(
                div()
                    .p_4()
                    .text_color(cx.theme().muted_foreground)
                    .child("Select an item"),
            );
        };

        let location = format!("{}/{}#{}", item.repo_owner, item.repo_name, item.number);
        let mut state_line = format!("{}  ·  @{}", item.state, item.author.as_deref().unwrap_or("ghost"));
        if item.is_draft {
            state_line.push_str("  ·  draft");
        }
        if let Some(decision) = &item.review_decision {
            state_line.push_str("  ·  ");
            state_line.push_str(decision);
        }

        container
            .p_4()
            .gap_2()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from(location)),
            )
            .child(
                div()
                    .text_lg()
                    .font_bold()
                    .text_color(cx.theme().foreground)
                    .child(SharedString::from(item.title.clone())),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from(state_line)),
            )
            .when(!item.labels.is_empty(), |e| {
                e.child(detail_field("labels", &item.labels.join(", "), cx))
            })
            .when_some(item.base_ref.as_ref().zip(item.head_ref.as_ref()), |e, (base, head)| {
                e.child(detail_field("branch", &format!("{head} → {base}"), cx))
            })
            .when(!item.assignees.is_empty(), |e| {
                e.child(detail_field("assignees", &item.assignees.join(", "), cx))
            })
            .when(!item.requested_reviewers.is_empty(), |e| {
                e.child(detail_field(
                    "reviewers",
                    &item.requested_reviewers.join(", "),
                    cx,
                ))
            })
            .when(!item.reviews.is_empty(), |e| {
                let reviews = item
                    .reviews
                    .iter()
                    .map(|(login, state)| format!("{login}: {state}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                e.child(detail_field("reviews", &reviews, cx))
            })
            .when_some(item.milestone.as_ref(), |e, m| {
                e.child(detail_field("milestone", m, cx))
            })
            .child(detail_field("updated", &item.updated_at, cx))
            .child(detail_field("url", &item.url, cx))
            .child({
                // Body rendered as Markdown via gpui-component's native TextView
                // (GFM + code highlighting, composites with gpui's GPU layers).
                // Left non-scrollable so the whole pane scrolls via `detail_scroll`.
                let body_box = div()
                    .mt_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(cx.theme().border);
                match item.body.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    Some(body) => body_box.child(markdown(body.to_string()).selectable(true)),
                    None => body_box
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("(no description)"),
                }
            })
    }

    /// Self-managed comments overlay (View comments). Rendered over the panes when
    /// `comments_open`; repaints when `CommentsLoaded` arrives. Keys are scoped to
    /// `COMMENTS_CONTEXT` via the focused panel.
    fn render_comments_overlay(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let hidden_count = self.comments.iter().filter(|c| c.is_minimized).count();
        let sort_label = if self.comments_sort_desc { "newest" } else { "oldest" };

        let body = if self.comments_loading {
            div()
                .p_4()
                .text_color(cx.theme().muted_foreground)
                .child("Loading comments…")
                .into_any_element()
        } else if self.comments.is_empty() {
            div()
                .p_4()
                .text_color(cx.theme().muted_foreground)
                .child("No comments.")
                .into_any_element()
        } else {
            let mut order: Vec<usize> = (0..self.comments.len()).collect();
            if self.comments_sort_desc {
                order.reverse();
            }
            let mut list = v_flex()
                .id("comments-scroll")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.comments_scroll)
                .p_3()
                .gap_3();
            for (pos, idx) in order.into_iter().enumerate() {
                let c = &self.comments[idx];
                let head = h_flex()
                    .gap_2()
                    .text_sm()
                    .child(
                        div()
                            .font_bold()
                            .text_color(cx.theme().foreground)
                            .child(SharedString::from(format!("@{}", c.author))),
                    )
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child(SharedString::from(c.created_at.clone())),
                    );
                let mut block = v_flex().gap_1().child(head);
                if c.is_minimized && !self.comments_show_hidden {
                    let reason = c.minimized_reason.clone().unwrap_or_else(|| "minimized".into());
                    block = block.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(SharedString::from(format!(
                                "▸ hidden ({reason}) — press h to expand"
                            ))),
                    );
                } else {
                    if c.is_minimized {
                        let reason =
                            c.minimized_reason.clone().unwrap_or_else(|| "minimized".into());
                        block = block.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().accent)
                                .child(SharedString::from(format!("⚠ hidden ({reason})"))),
                        );
                    }
                    block = block.child(markdown(c.body.clone()).selectable(true));
                }
                // Separator above every comment except the first.
                if pos > 0 {
                    block = block.pt_3().border_t_1().border_color(cx.theme().border);
                }
                list = list.child(block);
            }
            list.into_any_element()
        };

        let footer = format!(
            "Esc/q: close   j/k: scroll   g/G: top/bottom   s: {sort_label}   h: show/hide ({hidden_count})"
        );

        // Scrim: full-size, centers the panel, and swallows clicks to the panes.
        h_flex()
            .absolute()
            .inset_0()
            .p_8()
            .justify_center()
            .items_center()
            .bg(hsla(0., 0., 0., 0.5))
            .occlude()
            .child(
                v_flex()
                    .id("comments-overlay")
                    .track_focus(&self.comments_focus_handle)
                    .key_context(COMMENTS_CONTEXT)
                    .w(px(640.))
                    // Definite height (fills the scrim minus its padding) so the
                    // flex_1 body has room to expand and scroll. With only `max_h`
                    // the panel collapsed to its content height (tiny popup).
                    .h_full()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded_lg()
                    .shadow_lg()
                    .child(
                        div()
                            .w_full()
                            .flex_shrink_0()
                            .px_4()
                            .py_2()
                            .border_b_1()
                            .border_color(cx.theme().border)
                            .font_bold()
                            .text_color(cx.theme().foreground)
                            .child(self.comments_title.clone()),
                    )
                    .child(div().flex_1().min_h_0().child(body))
                    .child(
                        div()
                            .w_full()
                            .flex_shrink_0()
                            .px_4()
                            .py_2()
                            .border_t_1()
                            .border_color(cx.theme().border)
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(SharedString::from(footer)),
                    ),
            )
    }
}

/// Add a menu item whose click runs `f` against the `GlaucaApp` entity. Keeps the
/// click closures `'static` while still calling back into the view.
fn app_menu_item<F>(
    menu: PopupMenu,
    app: &Entity<GlaucaApp>,
    label: impl Into<SharedString>,
    f: F,
) -> PopupMenu
where
    F: Fn(&mut GlaucaApp, &mut Window, &mut Context<GlaucaApp>) + 'static,
{
    let app = app.clone();
    menu.item(
        PopupMenuItem::new(label.into()).on_click(move |_ev, window, cx| {
            app.update(cx, |this, cx| f(this, window, cx));
        }),
    )
}

/// Build the action menu for a given `MenuKind`, reusing `dispatch_action` and the
/// index-based entry helpers. Shared by right-click and the Enter action menu.
fn populate_menu(mut menu: PopupMenu, app: &Entity<GlaucaApp>, kind: MenuKind) -> PopupMenu {
    match kind {
        MenuKind::Item(item) => {
            for action in ItemAction::available_for(&item.kind) {
                let item = item.clone();
                let label = action.label().to_string();
                menu = app_menu_item(menu, app, label, move |this, window, cx| {
                    this.dispatch_action(action.clone(), item.clone(), window, cx);
                });
            }
        }
        MenuKind::Entry { index, is_query } => {
            menu = app_menu_item(menu, app, "Edit", move |this, window, cx| {
                this.edit_entry_at(index, window, cx);
            });
            menu = app_menu_item(menu, app, "Delete", move |this, _w, _cx| {
                this.delete_entry_at(index);
            });
            menu = app_menu_item(menu, app, "Move up", move |this, _w, _cx| {
                this.reorder_entry(index, false);
            });
            menu = app_menu_item(menu, app, "Move down", move |this, _w, _cx| {
                this.reorder_entry(index, true);
            });
            menu = menu.separator();
            if is_query {
                menu = app_menu_item(menu, app, "New filter stream", move |this, window, cx| {
                    this.new_filter_stream_under(index, window, cx);
                });
            }
            menu = app_menu_item(menu, app, "New query", |this, window, cx| {
                this.new_query(window, cx);
            });
        }
        MenuKind::NewQueryOnly => {
            menu = app_menu_item(menu, app, "New query", |this, window, cx| {
                this.new_query(window, cx);
            });
        }
    }
    menu
}

/// A `label: value` row in the detail pane.
fn detail_field(label: &str, value: &str, cx: &App) -> impl IntoElement {
    h_flex()
        .w_full()
        .gap_2()
        .text_sm()
        .child(
            div()
                .flex_shrink_0()
                .w(px(96.))
                .text_color(cx.theme().muted_foreground)
                .child(SharedString::from(label.to_string())),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(cx.theme().foreground)
                .child(SharedString::from(value.to_string())),
        )
}

/// Render an item title, emphasising the inline-filter match range if any.
fn highlight_title(title: &str, range: Option<(usize, usize)>, cx: &App) -> impl IntoElement {
    let base = h_flex()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .font_bold()
        .text_color(cx.theme().foreground);

    match range {
        Some((start, end)) if start < end && end <= title.len() => base
            .child(SharedString::from(title[..start].to_string()))
            .child(
                div()
                    .bg(cx.theme().accent)
                    .text_color(cx.theme().accent_foreground)
                    .child(SharedString::from(title[start..end].to_string())),
            )
            .child(SharedString::from(title[end..].to_string())),
        _ => base.truncate().child(SharedString::from(title.to_string())),
    }
}

impl Render for GlaucaApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let user = match &self.current_user {
            Some(u) => format!("connected as {u}"),
            None => "not authenticated".to_string(),
        };
        let mut status_bits = Vec::new();
        if self.syncing {
            status_bits.push("syncing…".to_string());
        }
        if self.bg_sync_pending > 0 {
            status_bits.push(format!("{} bg", self.bg_sync_pending));
        }
        if let Some(s) = &self.status {
            status_bits.push(s.clone());
        }
        let header = if status_bits.is_empty() {
            user
        } else {
            format!("{user}  ·  {}", status_bits.join("  ·  "))
        };

        v_flex()
            .id("glauca-root")
            .key_context(GLAUCA_CONTEXT)
            .track_focus(&self.focus_handle)
            // Track the pointer (no repaint) so the Enter action menu can anchor
            // near the cursor.
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, _cx| {
                this.last_pointer = ev.position;
            }))
            .on_action(cx.listener(Self::on_move_down))
            .on_action(cx.listener(Self::on_move_up))
            .on_action(cx.listener(Self::on_focus_left))
            .on_action(cx.listener(Self::on_focus_right))
            .on_action(cx.listener(Self::on_activate))
            .on_action(cx.listener(Self::on_focus_filter))
            .on_action(cx.listener(Self::on_cancel))
            .on_action(cx.listener(Self::on_quit))
            .on_action(cx.listener(Self::on_delete_entry))
            .on_action(cx.listener(Self::on_reorder_down))
            .on_action(cx.listener(Self::on_reorder_up))
            .on_action(cx.listener(Self::on_new_query))
            .on_action(cx.listener(Self::on_new_filter_stream))
            .on_action(cx.listener(Self::on_edit_entry))
            .on_action(cx.listener(Self::on_open_in_browser))
            .on_action(cx.listener(Self::on_open_comments))
            .on_action(cx.listener(Self::on_comments_scroll_down))
            .on_action(cx.listener(Self::on_comments_scroll_up))
            .on_action(cx.listener(Self::on_comments_top))
            .on_action(cx.listener(Self::on_comments_bottom))
            .on_action(cx.listener(Self::on_comments_toggle_sort))
            .on_action(cx.listener(Self::on_comments_toggle_hidden))
            .on_action(cx.listener(Self::on_comments_close))
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .w_full()
                    .flex_shrink_0()
                    .px_4()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from(header)),
            )
            .child(
                h_flex()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_left(cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(
                                // Inline filter input (drives `filter_items`).
                                div()
                                    .w_full()
                                    .flex_shrink_0()
                                    .p_2()
                                    .border_b_1()
                                    .border_color(cx.theme().border)
                                    .child(Input::new(&self.filter_input)),
                            )
                            .child(self.render_items(cx)),
                    )
                    .child(self.render_detail(cx)),
            )
            // Comments overlay draws over the 3-pane row (absolute, full-size).
            .when(self.comments_open, |this| {
                this.child(self.render_comments_overlay(cx))
            })
            // Right-click / Enter action menu, anchored at the click/pointer point.
            // A full-window backdrop swallows clicks and dismisses on outside click.
            .when_some(self.menu.clone(), |this, menu| {
                this.child(
                    deferred(
                        anchored().child(
                            div()
                                .w(window.bounds().size.width)
                                .h(window.bounds().size.height)
                                .occlude()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        this.menu = None;
                                        cx.notify();
                                    }),
                                )
                                .child(
                                    anchored()
                                        .position(self.menu_pos)
                                        .snap_to_window_with_margin(px(8.))
                                        .child(menu),
                                ),
                        ),
                    )
                    .with_priority(1),
                )
            })
            // gpui-component stores open dialogs/sheets/notifications in `Root`, but
            // `Root`'s own render does NOT paint them — the inner view must mount the
            // overlay layers (see examples/dialog_overlay). Without these, every
            // `open_dialog` (entry add/edit forms, action menu, comment/approve/merge)
            // is invisible, which is why the editing keys appeared to do nothing.
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_sheet_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}

fn main() -> Result<()> {
    // The engine runs on its own multi-thread tokio runtime; `rt` must outlive
    // the gpui event loop so its background tasks keep being driven.
    let rt = tokio::runtime::Runtime::new()?;
    let (engine, init) = rt.block_on(async {
        let db_path = db::default_db_path();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let pool = db::open_pool(&db_path).await?;
        let gh = github::build_client()?;
        Engine::start(pool, gh).await
    })?;

    gpui_platform::application().run(move |cx| {
        gpui_component::init(cx);

        // Navigation/edit keys are scoped to "Glauca && !Input": a bare "Glauca"
        // binding would still fire while a gpui-component Input is focused, because
        // dispatch bubbles to the root node (where the context is just [Glauca]) and
        // matches there — swallowing letters meant for the text box. The `!Input`
        // term is evaluated against the *full* focus path, so it disables these
        // bindings whenever an Input is anywhere in the chain. Escape stays plain
        // "Glauca" so it can blur the filter / close a dialog from inside the Input.
        cx.bind_keys([
            KeyBinding::new("j", MoveDown, Some(NAV_CONTEXT)),
            KeyBinding::new("k", MoveUp, Some(NAV_CONTEXT)),
            KeyBinding::new("down", MoveDown, Some(NAV_CONTEXT)),
            KeyBinding::new("up", MoveUp, Some(NAV_CONTEXT)),
            KeyBinding::new("h", FocusLeft, Some(NAV_CONTEXT)),
            KeyBinding::new("l", FocusRight, Some(NAV_CONTEXT)),
            KeyBinding::new("left", FocusLeft, Some(NAV_CONTEXT)),
            KeyBinding::new("right", FocusRight, Some(NAV_CONTEXT)),
            KeyBinding::new("enter", Activate, Some(NAV_CONTEXT)),
            KeyBinding::new("/", FocusFilter, Some(NAV_CONTEXT)),
            KeyBinding::new("escape", Cancel, Some(GLAUCA_CONTEXT)),
            KeyBinding::new("n", NewQuery, Some(NAV_CONTEXT)),
            KeyBinding::new("f", NewFilterStream, Some(NAV_CONTEXT)),
            KeyBinding::new("e", EditEntry, Some(NAV_CONTEXT)),
            KeyBinding::new("d", DeleteEntry, Some(NAV_CONTEXT)),
            KeyBinding::new("shift-j", ReorderDown, Some(NAV_CONTEXT)),
            KeyBinding::new("shift-k", ReorderUp, Some(NAV_CONTEXT)),
            KeyBinding::new("q", Quit, Some(NAV_CONTEXT)),
            KeyBinding::new("o", OpenInBrowser, Some(NAV_CONTEXT)),
            KeyBinding::new("c", OpenComments, Some(NAV_CONTEXT)),
            // Comments overlay controls (active only while the overlay is focused).
            KeyBinding::new("j", CommentsScrollDown, Some(COMMENTS_CONTEXT)),
            KeyBinding::new("k", CommentsScrollUp, Some(COMMENTS_CONTEXT)),
            KeyBinding::new("down", CommentsScrollDown, Some(COMMENTS_CONTEXT)),
            KeyBinding::new("up", CommentsScrollUp, Some(COMMENTS_CONTEXT)),
            KeyBinding::new("g", CommentsTop, Some(COMMENTS_CONTEXT)),
            KeyBinding::new("shift-g", CommentsBottom, Some(COMMENTS_CONTEXT)),
            KeyBinding::new("s", CommentsToggleSort, Some(COMMENTS_CONTEXT)),
            KeyBinding::new("h", CommentsToggleHidden, Some(COMMENTS_CONTEXT)),
            KeyBinding::new("q", CommentsClose, Some(COMMENTS_CONTEXT)),
        ]);

        cx.spawn(async move |cx| {
            cx.open_window(WindowOptions::default(), move |window, cx| {
                window.set_window_title("glauca");
                let view = cx.new(|cx| GlaucaApp::new(engine, init, window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("Failed to open window");
        })
        .detach();
    });

    // Keep the runtime alive across the whole GUI lifetime.
    drop(rt);
    Ok(())
}
