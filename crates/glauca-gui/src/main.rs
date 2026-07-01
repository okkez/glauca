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
use glauca_core::actions::{CustomAction, CustomActions};
use glauca_core::engine::{AppMessage, Engine, EngineCommand, EngineInit, ReviewEvent};
use glauca_core::filter::FilterQuery;
use glauca_core::logic::{
    ReviewState, compute_unread_counts, count_changed, expand_me, group_range, is_item_unread,
    move_group_down, query_label, reviewer_overlays,
};
use glauca_core::notify::ItemTracker;
use glauca_core::types::{
    CommentEntry, ItemAction, ItemEntry, LeftPaneEntry, MergeStrategy, UserRef,
};
use glauca_core::{db, github};
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::avatar::Avatar;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu, PopupMenu, PopupMenuItem};
use gpui_component::radio::RadioGroup;
use gpui_component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_component::text::{TextView, TextViewState, markdown};
use gpui_component::tooltip::Tooltip;
use gpui_component::{
    ActiveTheme, Root, Sizable, StyledExt, Theme, ThemeMode, WindowExt, h_flex, v_flex,
};
use smol::Timer;
use tokio::sync::mpsc::Sender;

mod assets;
mod settings;
use settings::{GuiSettings, ThemePreference};

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

/// Scroll a tracked `overflow_y_scroll` container by `delta_px` pixels (positive
/// = down). gpui's scroll offset goes negative downward, clamped to the content.
fn scroll_vertically(handle: &ScrollHandle, delta_px: f32) {
    let mut off = handle.offset();
    off.y -= px(delta_px);
    let min_y = -handle.max_offset().y;
    if off.y < min_y {
        off.y = min_y;
    }
    if off.y > px(0.) {
        off.y = px(0.);
    }
    handle.set_offset(off);
}

/// Wrap a pane's `content` in the standard frame: a neutral 1px border on the
/// left/right/bottom edges plus a top edge that turns `primary` when `focused`
/// — the keyboard-focus indicator. gpui colors a border uniformly, so the two
/// colors are split across an outer (top) element and an inner (other three).
fn pane_frame(focused: bool, content: impl IntoElement, cx: &App) -> Div {
    let top = if focused {
        cx.theme().primary
    } else {
        cx.theme().border
    };
    v_flex().size_full().border_t_1().border_color(top).child(
        v_flex()
            .size_full()
            .border_l_1()
            .border_r_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(content),
    )
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
        CopyUrl,
        RunCustomAction,
        Refresh,
        CommentsScrollDown,
        CommentsScrollUp,
        CommentsTop,
        CommentsBottom,
        CommentsToggleSort,
        CommentsToggleHidden,
        CommentsClose,
        SetThemeSystem,
        SetThemeLight,
        SetThemeDark,
        ToggleNotifications,
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
    Item(Box<ItemEntry>),
    /// Left-pane entry actions for the entry at `index`.
    Entry { index: usize, is_query: bool },
    /// Empty area of the left pane: only "New query".
    NewQueryOnly,
    /// User-defined custom actions for an item (opened with `x`), pre-filtered to
    /// the item's kind.
    CustomActions {
        item: Box<ItemEntry>,
        actions: Vec<CustomAction>,
    },
}

/// Inputs to `open_filter_stream_form` (add when `edit` is `None`, edit otherwise).
struct FilterStreamFormParams {
    edit: Option<i64>,
    parent_id: i64,
    kind: String,
    init_name: String,
    init_filter: String,
}

struct GlaucaApp {
    engine: Engine,
    /// Cloneable command sender, used from non-async click handlers.
    cmd_tx: Sender<EngineCommand>,

    entries: Vec<LeftPaneEntry>,
    entry_cursor: usize,
    current_user: Option<String>,
    /// Display name of the authenticated user (shown in the left-pane header).
    current_user_name: Option<String>,
    /// Avatar URL of the authenticated user (shown in the left-pane header).
    current_user_avatar_url: Option<String>,

    items: Vec<ItemEntry>,
    /// Indices into `items` passing the stream + inline filter. Cached so render
    /// (and the virtualized list) never re-scan all items per frame/keystroke;
    /// rebuilt by `recompute_filtered` only when items/filter/stream_filter change.
    filtered: Vec<usize>,
    /// Index into `filtered` of the row shown in the detail pane.
    item_cursor: usize,
    /// Inline filter text (mirrors the `filter_input` value; drives `recompute_filtered`).
    filter: String,
    unread_counts: HashMap<(bool, i64), usize>,
    /// Filter stream filter applied to the item list (None for root queries).
    stream_filter: Option<String>,

    /// Freshly-synced items for the currently-viewed query, held back from the
    /// list because they arrived from a background sync. Applied on explicit
    /// action (clicking the "N updated" banner). `None` when nothing is pending.
    pending_items: Option<Vec<ItemEntry>>,
    /// How many of `pending_items` are new/updated vs the displayed list (the
    /// number shown in the banner).
    pending_count: usize,

    /// Whether a manual GitHub sync is in progress for the selected query.
    syncing: bool,
    /// Number of pending background auto-refresh jobs (queued + in-progress).
    bg_sync_pending: usize,
    status: Option<String>,

    left_scroll: ScrollHandle,
    /// Virtualized, variable-height state for the center item list. Item rows
    /// wrap their titles and grow to fit, so `uniform_list` (uniform heights)
    /// can't be used; `list` measures per-item with overdraw. Kept in sync with
    /// `filtered.len()` by `recompute_filtered`.
    items_list: ListState,
    /// Drag-resizable left/center/right pane widths. Persisted to `GuiSettings`
    /// on every resize and restored on startup via `pane_sizes`.
    pane_state: Entity<ResizableState>,
    /// Saved pane widths (px), left-to-right, used to seed the initial pane
    /// sizes on startup. Empty on first run (falls back to defaults).
    pane_sizes: Vec<f32>,
    /// Parsed/virtualized state for the detail pane's Markdown body. Held as an
    /// entity so the body renders via a virtualized `gpui::list` (only the
    /// visible part is laid out), keeping pane-resize repaints cheap. Content is
    /// synced from the selected item in `render_detail` (a no-op when unchanged).
    detail_text: Entity<TextViewState>,
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

    /// Selected event for the open review dialog (Comment / Approve / Request
    /// Changes); reset to `Approve` each time the dialog opens.
    review_action: ReviewEvent,

    /// Inline filter input. Its `Change` events update `filter` (see `new`).
    filter_input: Entity<InputState>,
    /// Pending debounced re-filter task; replacing it cancels the previous one.
    filter_task: Option<Task<()>>,
    /// Selected color theme. Drives the View menu's active marker and decides
    /// whether window-appearance changes re-sync the theme (only in `System`).
    theme_pref: ThemePreference,
    /// Whether background-sync arrivals fire OS desktop notifications. Persisted
    /// to `GuiSettings`; toggled from the View menu.
    notifications_enabled: bool,
    /// Per-query session baseline for the notification "N updated" count, so the
    /// first load of each query establishes a baseline without notifying (no
    /// startup storm). See `glauca_core::notify::ItemTracker`.
    notif_tracker: ItemTracker,
    /// User-defined custom actions loaded from `actions.toml` (see
    /// `glauca_core::actions`). Offered via the `x` picker, filtered by kind.
    custom_actions: CustomActions,
    /// Keeps the `filter_input` subscription alive for the view's lifetime.
    _subscriptions: Vec<Subscription>,
}

impl GlaucaApp {
    fn new(engine: Engine, init: EngineInit, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Periodically drain engine messages and repaint while the window lives.
        // Use gpui's executor timer (not `smol::Timer`): its completion wakes the
        // platform event loop, so the loop keeps draining while the window is idle.
        // With `smol::Timer` the tick doesn't poke gpui's loop, so background-sync
        // messages sat undrained until the next user interaction (no "N updated"
        // banner appeared on its own).
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(DRAIN_INTERVAL).await;
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
        // change so `recompute_filtered` re-runs and the detail pane stays in range.
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
            current_user_name,
            current_user_avatar_url,
        } = init;
        let pane_state = cx.new(|_| ResizableState::default());
        let detail_text = cx.new(|cx| TextViewState::markdown("", cx));
        // `sync_interval_secs` is read separately in `main` (passed to the engine);
        // this view only needs the presentation fields.
        let GuiSettings {
            pane_sizes,
            theme,
            notifications_enabled,
            ..
        } = GuiSettings::load();
        let mut app = Self {
            engine,
            cmd_tx,
            entries,
            entry_cursor: 0,
            current_user,
            current_user_name,
            current_user_avatar_url,
            items: Vec::new(),
            filtered: Vec::new(),
            item_cursor: 0,
            filter: String::new(),
            unread_counts: HashMap::new(),
            stream_filter: None,
            pending_items: None,
            pending_count: 0,
            syncing: false,
            bg_sync_pending: 0,
            status: None,
            left_scroll: ScrollHandle::new(),
            items_list: ListState::new(0, ListAlignment::Top, px(120.)),
            pane_state,
            pane_sizes,
            detail_text,
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
            review_action: ReviewEvent::Approve,
            filter_input,
            filter_task: None,
            theme_pref: theme,
            notifications_enabled,
            notif_tracker: ItemTracker::new(),
            custom_actions: CustomActions::load(),
            _subscriptions: vec![subscription],
        };
        // Apply the saved theme up front (System follows the OS appearance).
        app.apply_theme(Some(window), cx);
        // While following the OS, re-sync whenever its appearance flips. The
        // closure re-reads `theme_pref` so pinning Light/Dark stops the follow.
        let this = cx.entity();
        let appearance_sub = window.observe_window_appearance(move |window, cx| {
            this.update(cx, |app, cx| {
                if app.theme_pref == ThemePreference::System {
                    // Re-apply via `apply_theme` so the GitHub dark overlay is
                    // re-applied when the OS flips to dark.
                    app.apply_theme(Some(window), cx);
                }
            });
        });
        app._subscriptions.push(appearance_sub);
        app.prime();
        // Restore saved column widths into the authoritative ResizableState after
        // the first frame is drawn (panels are synced and the container has a real
        // size by then). The `.size()` initial_size hints on the panels lose to
        // `adjust_to_container_size`, which overwrites `panel.size` on that first
        // prepaint — so seed the widths explicitly here. `on_next_frame` is a
        // one-shot, so no guard flag is needed.
        let pane_state = app.pane_state.clone();
        let left = app.pane_sizes.first().copied();
        let right = app.pane_sizes.get(2).copied();
        if left.is_some() || right.is_some() {
            window.on_next_frame(move |window, cx| {
                pane_state.update(cx, |state, cx| {
                    // panels.len() == 3 and the container size is settled here;
                    // out-of-range indices are a no-op. Apply left (ix 0) before
                    // right (ix 2); both take from the flexible center pane.
                    if let Some(w) = left {
                        state.resize_panel(0, px(w), window, cx);
                    }
                    if let Some(w) = right {
                        state.resize_panel(2, px(w), window, cx);
                    }
                });
            });
        }
        // Grab keyboard focus so single-letter navigation works without a click.
        app.focus_handle.focus(window, cx);
        app
    }

    /// Apply `self.theme_pref` to the global gpui-component theme. `System`
    /// follows the OS appearance; `Light`/`Dark` pin an explicit mode. When the
    /// resolved mode is dark, overlay the GitHub-flavored palette (the stock
    /// dark theme is near-black) — see `apply_github_dark_overlay`.
    fn apply_theme(&self, window: Option<&mut Window>, cx: &mut App) {
        match self.theme_pref {
            ThemePreference::System => Theme::sync_system_appearance(window, cx),
            ThemePreference::Light => Theme::change(ThemeMode::Light, window, cx),
            ThemePreference::Dark => Theme::change(ThemeMode::Dark, window, cx),
        }
        if cx.theme().mode.is_dark() {
            apply_github_dark_overlay(cx);
        }
    }

    /// Switch the theme from the View menu: apply it, persist the choice
    /// (preserving the other settings), and repaint.
    fn set_theme(&mut self, pref: ThemePreference, window: &mut Window, cx: &mut Context<Self>) {
        self.theme_pref = pref;
        self.apply_theme(Some(window), cx);
        let mut settings = GuiSettings::load();
        settings.theme = pref;
        settings.save();
        cx.notify();
    }

    fn on_set_theme_system(
        &mut self,
        _: &SetThemeSystem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_theme(ThemePreference::System, window, cx);
    }

    fn on_set_theme_light(
        &mut self,
        _: &SetThemeLight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_theme(ThemePreference::Light, window, cx);
    }

    fn on_set_theme_dark(&mut self, _: &SetThemeDark, window: &mut Window, cx: &mut Context<Self>) {
        self.set_theme(ThemePreference::Dark, window, cx);
    }

    /// Toggle desktop notifications from the View menu: flip the flag, persist it
    /// (preserving the other settings), and repaint the menu marker.
    fn toggle_notifications(&mut self, cx: &mut Context<Self>) {
        self.notifications_enabled = !self.notifications_enabled;
        let mut settings = GuiSettings::load();
        settings.notifications_enabled = self.notifications_enabled;
        settings.save();
        cx.notify();
    }

    fn on_toggle_notifications(
        &mut self,
        _: &ToggleNotifications,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_notifications(cx);
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
    fn send(&self, cmd: EngineCommand) {
        let _ = self.cmd_tx.try_send(cmd);
    }

    fn selected_root_query_id(&self) -> Option<i64> {
        self.entries
            .get(self.entry_cursor)
            .map(|e| e.root_query_id())
    }

    /// Display name of the selected left-pane entry (query label or stream name),
    /// shown in the center pane header.
    fn selected_entry_label(&self) -> Option<String> {
        self.entries.get(self.entry_cursor).map(|e| match e {
            LeftPaneEntry::Query(q) => q.label.clone(),
            LeftPaneEntry::FilterStream(fs) => fs.name.clone(),
        })
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
        self.clear_pending();
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
        let stream_filter = entry.stream_filter().map(|s| s.to_string());
        self.entry_cursor = index;
        self.items.clear();
        self.item_cursor = 0;
        self.stream_filter = stream_filter;
        self.clear_pending();
        self.recompute_filtered();
        self.send(EngineCommand::LoadCached { query_id: root_id });
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
                    self.items_list.scroll_to_reveal_item(self.item_cursor);
                    self.mark_current_item_read(cx);
                }
            }
            // The detail body owns its own (virtualized) scroll — use the mouse
            // wheel / scrollbar. j/k here is a no-op.
            Focus::ItemDetail => {}
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
                    self.items_list.scroll_to_reveal_item(self.item_cursor);
                    self.mark_current_item_read(cx);
                }
            }
            // See `on_move_down`: the detail body scrolls via mouse/scrollbar.
            Focus::ItemDetail => {}
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
    fn on_open_in_browser(
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
    fn on_copy_url(&mut self, _: &CopyUrl, _window: &mut Window, cx: &mut Context<Self>) {
        if self.focus == Focus::QueryList {
            return;
        }
        if let Some(item) = self.selected_item() {
            cx.write_to_clipboard(ClipboardItem::new_string(item.url));
        }
    }

    /// `x` — open the custom-action picker for the selected item, anchored near
    /// the last pointer position (mirroring the Enter action menu). No-op with a
    /// status hint when no defined action applies to the item's kind.
    fn on_run_custom_action(
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
        let actions: Vec<CustomAction> = self
            .custom_actions
            .for_kind(&item.kind)
            .into_iter()
            .cloned()
            .collect();
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

    /// The query string of the root query with `root_id` (found in the left-pane
    /// entries) — needed to re-sync the list backing a filter stream, which has
    /// no query string of its own.
    fn root_query_str_for(&self, root_id: i64) -> Option<String> {
        self.entries.iter().find_map(|e| match e {
            LeftPaneEntry::Query(q) if q.id == root_id => Some(q.query_str.clone()),
            _ => None,
        })
    }

    /// `r` — context-sensitive refresh: re-sync the selected list when the left
    /// pane is focused, otherwise re-fetch just the selected item.
    fn on_refresh(&mut self, _: &Refresh, _window: &mut Window, cx: &mut Context<Self>) {
        if self.focus == Focus::QueryList {
            self.refresh_selected_list();
        } else {
            self.refresh_selected_item();
        }
        cx.notify();
    }

    /// Re-sync the list for the selected entry (its root query) in place, keeping
    /// the current selection.
    fn refresh_selected_list(&mut self) {
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
    fn refresh_selected_item(&mut self) {
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

    /// Mark every unread item of the entry at `index` read. For a query that is the
    /// whole root query; for a filter stream only its matching items (filter expanded
    /// with the current user here, since the engine does not know `@me`). The engine
    /// persists and reloads the query, which refreshes the unread badges via the
    /// `ItemsLoaded` handler — so this works for non-selected entries too.
    fn mark_all_read_at(&mut self, index: usize) {
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let query_id = entry.root_query_id();
        let filter = entry
            .stream_filter()
            .map(|f| expand_me(self.current_user.as_deref(), f).into_owned());
        self.send(EngineCommand::MarkAllRead { query_id, filter });
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
                stream_q.as_ref().is_none_or(|q| q.matches(item))
                    && (inline_q.is_empty() || inline_q.matches(item))
            })
            .map(|(ix, _)| ix)
            .collect();
        if self.item_cursor >= self.filtered.len() {
            self.item_cursor = self.filtered.len().saturating_sub(1);
        }
        // Keep the virtualized list's item count in step with `filtered`; a
        // mismatch makes `list` panic or render stale rows. This is the single
        // chokepoint every `filtered` mutation flows through.
        self.items_list.reset(self.filtered.len());
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
    fn new_filter_stream_under(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let parent_id = entry.root_query_id();
        let kind = entry.kind().to_string();
        self.open_filter_stream_form(
            FilterStreamFormParams {
                edit: None,
                parent_id,
                kind,
                init_name: String::new(),
                init_filter: String::new(),
            },
            window,
            cx,
        );
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
            self.open_filter_stream_form(
                FilterStreamFormParams {
                    edit: Some(id),
                    parent_id: parent,
                    kind,
                    init_name: name,
                    init_filter: filter,
                },
                window,
                cx,
            );
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
        let title = if edit.is_some() {
            "Edit query"
        } else {
            "Add query"
        };
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
                                Some(id) => {
                                    app.send(EngineCommand::EditQuery { id, name, query: q })
                                }
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
        params: FilterStreamFormParams,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let FilterStreamFormParams {
            edit,
            parent_id,
            kind,
            init_name,
            init_filter,
        } = params;
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("display name")
                .default_value(init_name)
        });
        let filter = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("filter (e.g. is:pr is:draft assignee:name)")
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
        // cleared per-item as items are read. Unread is now derived per item from
        // `updated_at` vs `last_read_updated_at`, so there is no per-entry baseline.
        self.stream_filter = entry.stream_filter().map(|s| s.to_string());

        let root_id = entry.root_query_id();
        self.send(EngineCommand::LoadCached { query_id: root_id });
        if entry.is_filter_stream() {
            return None;
        }

        let query_str = entry.root_query_str().unwrap_or_default().to_string();
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

    /// Force a full re-fetch of the current entry's root query (ignores
    /// `last_fetched_at`), which re-pages everything and prunes cached items that
    /// no longer match. Useful to reconcile after items have fallen out of the
    /// query (e.g. merged PRs lingering in an `is:open` list).
    fn full_resync_current(&mut self) {
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
        let menu = PopupMenu::build(window, cx, move |menu, _w, _cx| {
            populate_menu(menu, &app, kind)
        });
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
            ItemAction::OpenBrowser => self.send(EngineCommand::OpenBrowser {
                item: Box::new(item),
            }),
            ItemAction::Comment => self.open_comment_dialog(item, window, cx),
            ItemAction::ApprovePR => self.open_review_dialog(item, window, cx),
            ItemAction::MergePR => self.open_merge_dialog(item, window, cx),
            ItemAction::ViewComments => self.open_comments(item, window, cx),
            ItemAction::CopyUrl => cx.write_to_clipboard(ClipboardItem::new_string(item.url)),
            ItemAction::RefreshItem => {
                let number = item.number;
                if let Some(query_id) = self.selected_root_query_id() {
                    self.send(EngineCommand::RefreshItem {
                        query_id,
                        repo_owner: item.repo_owner,
                        repo_name: item.repo_name,
                        number,
                    });
                    self.status = Some(format!("Refreshing #{number}…"));
                }
            }
            // octorus is a terminal TUI launched only from the CLI front-end; it
            // is never offered in the GUI menu, so this arm is unreachable.
            ItemAction::ReviewOctorus => {}
        }
    }

    fn open_comment_dialog(
        &mut self,
        item: ItemEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
            dlg.title("Comment")
                .w(px(560.))
                .content(move |content, _w, _cx| content.child(Input::new(&body_c).h(px(220.))))
                .on_ok(move |_, _w, cx| {
                    let b = body_ok.read(cx).value().to_string();
                    let b = b.trim().to_string();
                    if !b.is_empty()
                        && let Some(app) = this.upgrade()
                    {
                        let item = item.clone();
                        app.update(cx, |app, _| {
                            app.send(EngineCommand::Comment {
                                url: item.url.clone(),
                                kind: item.kind.clone(),
                                body: b,
                            })
                        });
                    }
                    true
                })
        });
    }

    /// Submit a PR review: pick Comment / Approve / Request changes, type an
    /// optional body, and Cancel / Submit. Radio order matches `review_action`'s
    /// index mapping below. Opened by the "Approve PR" menu item (defaults to
    /// Approve). Uses explicit buttons (not `on_ok`) so the actions are visible.
    fn open_review_dialog(&mut self, item: ItemEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.review_action = ReviewEvent::Approve;
        let body = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(3, 12)
                .placeholder("Review comment (required for Comment / Request changes)")
        });
        let this = cx.weak_entity();
        window.open_dialog(cx, move |dlg, _w, _cx| {
            let body_render = body.clone();
            let body_submit = body.clone();
            let this = this.clone();
            let item = item.clone();
            dlg.title("Submit review")
                .w(px(560.))
                .content(move |content, _w, cx| {
                    // Radio order ⇄ ReviewEvent: 0 Comment, 1 Approve, 2 Request changes.
                    let selected = this.upgrade().map(|app| match app.read(cx).review_action {
                        ReviewEvent::Comment => 0,
                        ReviewEvent::Approve => 1,
                        ReviewEvent::RequestChanges => 2,
                    });
                    let radios = RadioGroup::horizontal("review-action")
                        .children(["Comment", "Approve", "Request changes"])
                        .selected_index(selected)
                        .on_click({
                            let this = this.clone();
                            move |ix, _w, cx| {
                                let event = match *ix {
                                    0 => ReviewEvent::Comment,
                                    2 => ReviewEvent::RequestChanges,
                                    _ => ReviewEvent::Approve,
                                };
                                if let Some(app) = this.upgrade() {
                                    app.update(cx, |app, cx| {
                                        app.review_action = event;
                                        cx.notify();
                                    });
                                }
                            }
                        });
                    let buttons = h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("review-cancel")
                                .ghost()
                                .label("Cancel")
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("review-submit")
                                .primary()
                                .label("Submit review")
                                .on_click({
                                    let this = this.clone();
                                    let item = item.clone();
                                    let body_submit = body_submit.clone();
                                    move |_, window, cx| {
                                        let Some(app) = this.upgrade() else { return };
                                        let event = app.read(cx).review_action;
                                        let b = body_submit.read(cx).value().trim().to_string();
                                        // gh requires a body for comment / request-changes reviews.
                                        if event.requires_body() && b.is_empty() {
                                            app.update(cx, |app, cx| {
                                                app.status = Some(
                                        "Review comment required for Comment / Request changes"
                                            .to_string(),
                                    );
                                                cx.notify();
                                            });
                                            return;
                                        }
                                        let body = if b.is_empty() { None } else { Some(b) };
                                        window.close_dialog(cx);
                                        app.update(cx, |app, _| {
                                            app.send(EngineCommand::SubmitReview {
                                                url: item.url.clone(),
                                                event,
                                                body,
                                            })
                                        });
                                    }
                                }),
                        );
                    content
                        .gap_3()
                        .child(radios)
                        .child(Input::new(&body_render).h(px(180.)))
                        .child(buttons)
                })
        });
    }

    fn open_merge_dialog(&mut self, item: ItemEntry, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        window.open_dialog(cx, move |dlg, _w, _cx| {
            let this = this.clone();
            let item = item.clone();
            dlg.title("Merge strategy")
                .w(px(320.))
                .content(move |content, _w, _cx| {
                    let mut col = content.gap_2();
                    for (ix, strat) in MergeStrategy::all().into_iter().enumerate() {
                        let label = strat.label().to_string();
                        let item = item.clone();
                        let this = this.clone();
                        col = col.child(Button::new(("merge", ix)).label(label).on_click(
                            move |_, window, cx| {
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
                            },
                        ));
                    }
                    col
                })
        });
    }

    /// Install `items` as the visible list. Caller refilters / notifies. `is_new`
    /// (unread) is already set per item by `cached_item_to_item_entry` when the
    /// engine builds them, so there is nothing to recompute here.
    fn apply_items_to_view(&mut self, items: Vec<ItemEntry>) {
        self.items = items;
    }

    /// Drop any held-back background-sync results / banner.
    fn clear_pending(&mut self) {
        self.pending_items = None;
        self.pending_count = 0;
    }

    /// Apply the stashed background-sync results to the visible list (banner
    /// click / explicit refresh). No-op when nothing is pending.
    fn apply_pending(&mut self, cx: &mut Context<Self>) {
        let Some(items) = self.pending_items.take() else {
            return;
        };
        self.pending_count = 0;
        if let Some(qid) = self.selected_root_query_id() {
            self.recompute_unread(qid, &items);
        }
        self.apply_items_to_view(items);
        self.recompute_filtered();
        cx.notify();
    }

    fn recompute_unread(&mut self, query_id: i64, items: &[ItemEntry]) {
        for (key, unread) in
            compute_unread_counts(&self.entries, query_id, items, self.current_user.as_deref())
        {
            self.unread_counts.insert(key, unread);
        }
    }

    /// Mark the currently-selected item read (it is shown in the detail pane):
    /// record the `updated_at` it was read at, clear its in-memory `is_new`,
    /// recompute the current query's unread badges, and persist via the engine
    /// (fire-and-forget). No-op if there is no selection or the item is already read
    /// (not currently unread).
    fn mark_current_item_read(&mut self, cx: &mut Context<Self>) {
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
    /// message handling (crates/glauca-tui/src/tui/mod.rs).
    fn apply(&mut self, msg: AppMessage, cx: &mut Context<Self>) {
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
                        self.notifications_enabled,
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
                    // and surface a "N updated" banner; applied on explicit action.
                    // Unread badges are deferred too, so nothing moves until then.
                    let n = count_changed(&self.items, &items);
                    if n == 0 {
                        self.clear_pending();
                    } else {
                        self.pending_items = Some(items);
                        self.pending_count = n;
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

    fn render_left(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        // Fixed header: current user's avatar + login (line 1) + display name
        // (line 2). Stays pinned while the entry list below scrolls.
        let mut avatar = Avatar::new().with_size(px(HEADER_AVATAR_PX));
        if let Some(login) = &self.current_user {
            avatar = avatar.name(login.clone());
        }
        if let Some(url) = &self.current_user_avatar_url {
            avatar = avatar.src(sized_avatar_url(url, HEADER_AVATAR_PX));
        }
        let login_line = self
            .current_user
            .clone()
            .unwrap_or_else(|| "not authenticated".to_string());
        let header = h_flex()
            .w_full()
            .flex_shrink_0()
            .px_3()
            .py_2()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(avatar)
            .child(
                v_flex()
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .text_color(cx.theme().sidebar_foreground)
                            .child(SharedString::from(login_line)),
                    )
                    .when_some(self.current_user_name.clone(), |e, name| {
                        e.child(
                            div()
                                .truncate()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(SharedString::from(name)),
                        )
                    }),
            );

        // Scrollable entry list (root queries + filter streams).
        let mut col = v_flex()
            .id("left-pane")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.left_scroll);

        for (i, entry) in self.entries.iter().enumerate() {
            let selected = i == self.entry_cursor;
            let is_stream = entry.is_filter_stream();
            let is_query = matches!(entry, LeftPaneEntry::Query(_));
            let label = match entry {
                LeftPaneEntry::Query(q) => q.label.clone(),
                LeftPaneEntry::FilterStream(fs) => fs.name.clone(),
            };
            let unread = self
                .unread_counts
                .get(&entry.unread_key())
                .copied()
                .unwrap_or(0);

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
        // It also pushes the status footer below to the bottom of the pane.
        col = col.child(
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
        );

        // Status footer: sync state and the latest status message (the user
        // identity now lives in the header). Only shown when there is something
        // to report.
        let mut sync_bits = Vec::new();
        if self.syncing {
            sync_bits.push("syncing…".to_string());
        }
        if self.bg_sync_pending > 0 {
            sync_bits.push(format!("{} bg", self.bg_sync_pending));
        }
        let has_footer = !sync_bits.is_empty() || self.status.is_some();
        let footer = has_footer.then(|| {
            let mut footer = v_flex()
                .w_full()
                .flex_shrink_0()
                .px_3()
                .py_2()
                .gap_0p5()
                .border_t_1()
                .border_color(cx.theme().border)
                .text_xs()
                .text_color(cx.theme().muted_foreground);
            if !sync_bits.is_empty() {
                footer = footer.child(SharedString::from(sync_bits.join("  ")));
            }
            if let Some(s) = &self.status {
                footer = footer.child(SharedString::from(s.clone()));
            }
            footer
        });

        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .child(header)
            .child(col)
            .children(footer)
    }

    /// Center pane content: the selected entry's name, the inline filter input,
    /// and the (virtualized) item list. The pane frame is added by the caller.
    fn render_center(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        v_flex()
            .size_full()
            .child(
                // Header: name of the selected query / stream.
                div()
                    .w_full()
                    .flex_shrink_0()
                    .px_3()
                    .py_1p5()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .truncate()
                    .text_color(cx.theme().foreground)
                    .child(SharedString::from(
                        self.selected_entry_label().unwrap_or_default(),
                    )),
            )
            .child(
                // Inline filter input (drives `recompute_filtered`).
                div()
                    .w_full()
                    .flex_shrink_0()
                    .p_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(Input::new(&self.filter_input)),
            )
            // "N updated" banner: shown when a background sync brought fresh
            // results for this query that we held back. Click to apply them.
            .when(self.pending_count > 0, |this| {
                let view = cx.entity();
                let n = self.pending_count;
                // Solid attention color (amber) with its matching foreground so the
                // banner clearly stands out instead of blending into the pane.
                let bg = cx.theme().warning;
                let fg = cx.theme().warning_foreground;
                let mut hover_bg = bg;
                hover_bg.l = (hover_bg.l + 0.06).min(1.0);
                this.child(
                    div()
                        .id("pending-refresh")
                        .w_full()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap_1()
                        .px_3()
                        .py_2()
                        .bg(bg)
                        .text_sm()
                        .font_bold()
                        .text_color(fg)
                        .cursor_pointer()
                        .hover(move |e| e.bg(hover_bg))
                        .on_click(move |_, _window, cx| {
                            view.update(cx, |this, cx| this.apply_pending(cx));
                        })
                        .child(SharedString::from(format!(
                            "↻  {n} updated — click to refresh"
                        ))),
                )
            })
            .child(self.render_items(cx))
    }

    fn render_items(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // `filtered` is precomputed (`recompute_filtered`); rows are built lazily
        // per visible range by `list`, which measures variable-height rows
        // (wrapped titles) with overdraw. `items_list`'s count is kept in sync
        // by `recompute_filtered`, so indices here always map into `filtered`.
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

        // The `list` render closure only receives `&App`, so it reads the view
        // entity for row data and captures it for click handlers (which mutate
        // via `update` since `cx.listener` is unavailable here).
        let view = cx.entity();
        // Parse the inline filter once per render (used to highlight matching
        // title text) and share it across all visible rows — re-parsing it per
        // row is wasted work, especially while a divider drag re-measures the
        // visible rows every frame.
        let fq = std::rc::Rc::new(FilterQuery::parse(&expand_me(
            self.current_user.as_deref(),
            &self.filter,
        )));
        container.child(
            list(self.items_list.clone(), move |ix, _window, cx| {
                view.read(cx).render_item_row(ix, &fq, &view, cx)
            })
            .flex_1(),
        )
    }

    /// Build one center-list row: optional `NEW` badge, a wrapping (multi-line)
    /// title, and a single-line meta line. Height is intentionally unconstrained
    /// so `list` grows the row to fit a wrapped title.
    fn render_item_row(
        &self,
        ix: usize,
        fq: &FilterQuery,
        view: &Entity<Self>,
        cx: &App,
    ) -> AnyElement {
        let Some(item) = self.filtered.get(ix).and_then(|&i| self.items.get(i)) else {
            return div().into_any_element();
        };
        // State is shown by the status icon, and the author by the avatar row
        // below, so both are dropped from this line.
        let mut meta = format!("{}/{}#{}", item.repo_owner, item.repo_name, item.number);
        if !item.labels.is_empty() {
            meta.push_str("  ·  ");
            meta.push_str(&item.labels.join(", "));
        }

        // Participants row (above the repo/meta line): author then assignees on
        // the left, reviewers (with review-state overlays) on the right.
        let reviewers = reviewer_overlays(item);
        let assignee_extra = item.assignees.len().saturating_sub(AVATAR_LIMIT);
        let reviewer_extra = reviewers.len().saturating_sub(AVATAR_LIMIT);
        let has_participants = item.author.is_some()
            || !item.assignees.is_empty()
            || !reviewers.is_empty()
            || item.comment_count > 0;

        let is_new = item.is_new;
        let selected = ix == self.item_cursor;
        let title_el = highlight_title(&item.title, fq.highlight_range(&item.title), cx);

        v_flex()
            .id(ix)
            .w_full()
            .px_4()
            .py_2()
            .gap_0p5()
            .border_b_1()
            .border_color(cx.theme().border)
            .cursor_pointer()
            .when(selected, |e| e.bg(cx.theme().list_active))
            // Unread rows get a faint background tint (replaces the old NEW
            // badge); selection still takes precedence.
            .when(!selected && is_new, |e| {
                let mut tint = cx.theme().accent;
                tint.a = 0.10;
                e.bg(tint)
            })
            .when(!selected, |e| e.hover(|e| e.bg(cx.theme().list_hover)))
            .on_click({
                let view = view.clone();
                move |event: &ClickEvent, _window, cx: &mut App| {
                    // Shift+click opens the row in the browser (mouse-only
                    // equivalent of the `o` key), in addition to selecting it.
                    let shift = event.modifiers().shift;
                    view.update(cx, |this, cx| {
                        this.focus = Focus::ItemList;
                        this.item_cursor = ix;
                        this.mark_current_item_read(cx);
                        if shift && let Some(item) = this.selected_item() {
                            this.send(EngineCommand::OpenBrowser {
                                item: Box::new(item),
                            });
                        }
                        cx.notify();
                    });
                }
            })
            .on_mouse_down(MouseButton::Right, {
                let view = view.clone();
                move |ev: &MouseDownEvent, window, cx: &mut App| {
                    view.update(cx, |this, cx| {
                        this.focus = Focus::ItemList;
                        this.item_cursor = ix;
                        if let Some(item) = this.selected_item() {
                            this.open_menu(ev.position, MenuKind::Item(Box::new(item)), window, cx);
                        }
                        cx.notify();
                    });
                }
            })
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    // Top-align so the status icon sits beside the first line
                    // when the title wraps to multiple lines.
                    .items_start()
                    .child(item_state_icon(item, cx))
                    .child(title_el),
            )
            .when(has_participants, |e| {
                e.child(
                    h_flex()
                        .w_full()
                        .justify_between()
                        .items_center()
                        .gap_2()
                        // Left: author, then assignees (+N overflow).
                        .child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .flex_shrink_0()
                                .when_some(item.author.as_ref(), |e, a| e.child(user_avatar(a)))
                                // Arrow reads "author → assignee(s)" when both sides exist.
                                .when(item.author.is_some() && !item.assignees.is_empty(), |e| {
                                    e.child(
                                        div()
                                            .flex_shrink_0()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("→"),
                                    )
                                })
                                .children(item.assignees.iter().take(AVATAR_LIMIT).map(user_avatar))
                                .when(assignee_extra > 0, |e| {
                                    e.child(avatar_overflow(assignee_extra, cx))
                                }),
                        )
                        // Right: reviewers with review-state overlay (+N overflow),
                        // then the comment count (octicon + number) when nonzero.
                        .child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .flex_shrink_0()
                                .children(
                                    reviewers
                                        .iter()
                                        .take(AVATAR_LIMIT)
                                        .map(|(u, s)| reviewer_avatar(u, *s, cx)),
                                )
                                .when(reviewer_extra > 0, |e| {
                                    e.child(avatar_overflow(reviewer_extra, cx))
                                })
                                .when(item.comment_count > 0, |e| {
                                    e.child(
                                        h_flex()
                                            .gap_0p5()
                                            .items_center()
                                            .flex_shrink_0()
                                            .child(
                                                svg()
                                                    .path("octicons/comment.svg")
                                                    .size_3()
                                                    .flex_shrink_0()
                                                    .text_color(cx.theme().muted_foreground),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(SharedString::from(
                                                        item.comment_count.to_string(),
                                                    )),
                                            ),
                                    )
                                }),
                        ),
                )
            })
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    // Private repos get a lock glyph ahead of the "owner/name" text.
                    .when(item.repo_private, |e| {
                        e.child(
                            svg()
                                .path("octicons/lock.svg")
                                .size_3()
                                .flex_shrink_0()
                                .text_color(cx.theme().muted_foreground),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(SharedString::from(meta)),
                    )
                    // Relative update time, right-aligned at the row's end.
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(SharedString::from(glauca_core::time::format_relative_time(
                                &item.updated_at,
                            ))),
                    ),
            )
            .into_any_element()
    }

    fn render_detail(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        // The pane frame (border + focus highlight) is added by the caller via
        // `pane_frame`, so the early returns below don't each have to wrap
        // themselves.
        let container = v_flex()
            .id("detail-pane")
            .size_full()
            .bg(cx.theme().background)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    if let Some(item) = this.selected_item() {
                        this.open_menu(ev.position, MenuKind::Item(Box::new(item)), window, cx);
                    }
                }),
            );

        let Some(item) = self
            .filtered
            .get(self.item_cursor)
            .and_then(|&i| self.items.get(i))
        else {
            return container.child(
                div()
                    .p_4()
                    .text_color(cx.theme().muted_foreground)
                    .child("Select an item"),
            );
        };

        let (state_path, state_color) = item_state_icon_info(item, cx);

        // Pinned header: metadata stays visible while the body scrolls. It can't
        // share a scroll region with the body because the body owns its own
        // virtualized scroll (see below), so it's a `flex_none` block on top.
        let header = v_flex()
            .flex_none()
            .p_4()
            .gap_2()
            // Title line: author avatar + state pill + (PR) review-decision icon +
            // the item title (which wraps; the leading items stay on the top line).
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_start()
                    // Leading controls kept in a vertically-centered cluster so the
                    // avatar, state pill, and review icon line up with each other;
                    // the cluster as a whole sits on the title's first line.
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .flex_shrink_0()
                            .when_some(item.author.clone(), |e, a| {
                                let login = a.login.clone();
                                e.child(
                                    div()
                                        .id("detail-author")
                                        .flex_shrink_0()
                                        .child(user_avatar(&a))
                                        .tooltip(move |window, cx| {
                                            Tooltip::new(login.clone()).build(window, cx)
                                        }),
                                )
                            })
                            // GitHub-style state pill: colored, rounded, light text.
                            .child(
                                h_flex()
                                    .gap_1()
                                    .items_center()
                                    .flex_shrink_0()
                                    .px_2()
                                    .py_0p5()
                                    .rounded_full()
                                    .bg(state_color)
                                    .text_color(white())
                                    .child(
                                        svg()
                                            .path(state_path)
                                            .size_3()
                                            .flex_shrink_0()
                                            .text_color(white()),
                                    )
                                    .child(div().text_xs().child(state_label(item))),
                            )
                            // Review decision as an icon with a tooltip (PRs only).
                            .when_some(item.review_decision.clone(), |e, decision| {
                                let (icon, color, label) = review_decision_icon(&decision, cx);
                                e.child(
                                    div()
                                        .id("review-decision")
                                        .flex_shrink_0()
                                        .child(
                                            svg()
                                                .path(icon)
                                                .size_5()
                                                .flex_shrink_0()
                                                .text_color(color),
                                        )
                                        .tooltip(move |window, cx| {
                                            Tooltip::new(label).build(window, cx)
                                        }),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_lg()
                            .font_bold()
                            .text_color(cx.theme().foreground)
                            .child(SharedString::from(item.title.clone())),
                    ),
            )
            .when(!item.labels.is_empty(), |e| {
                e.child(detail_field("labels", &item.labels.join(", "), cx))
            })
            .when_some(
                item.base_ref.as_ref().zip(item.head_ref.as_ref()),
                |e, (base, head)| e.child(detail_field("branch", &format!("{head} → {base}"), cx)),
            )
            .when(!item.assignees.is_empty(), |e| {
                e.child(detail_people_field(
                    "assignees",
                    item.assignees.iter().cloned().map(|u| user_chip(u, cx)),
                    cx,
                ))
            })
            .map(|e| {
                // Unified reviewers row: requested ∪ reviewed, state shown by the
                // avatar overlay (replaces the old separate reviewers/reviews rows).
                let reviewers = reviewer_overlays(item);
                e.when(!reviewers.is_empty(), |e| {
                    e.child(detail_people_field(
                        "reviewers",
                        reviewers.into_iter().map(|(u, s)| reviewer_chip(u, s, cx)),
                        cx,
                    ))
                })
            })
            .when_some(item.milestone.as_ref(), |e, m| {
                e.child(detail_field("milestone", m, cx))
            })
            .when_some(item.created_at_item.as_deref(), |e, created| {
                e.child(detail_field(
                    "created",
                    &glauca_core::time::format_local_datetime(created),
                    cx,
                ))
            })
            .child(detail_field(
                "updated",
                &glauca_core::time::format_local_datetime(&item.updated_at),
                cx,
            ));

        // Body rendered as Markdown via gpui-component's `TextView`, in its
        // virtualized `scrollable(true)` mode: only the visible part is laid out
        // each frame, so resizing the pane (which changes the wrap width) stays
        // cheap. The body owns its own scroll (mouse wheel / scrollbar). Content
        // is synced into the retained `detail_text` state; `set_text` is a no-op
        // unless the selected item's body actually changed.
        let body = match item
            .body
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(body) => {
                self.detail_text
                    .update(cx, |state, cx| state.set_text(body, cx));
                div()
                    .flex_1()
                    .min_h_0()
                    .px_4()
                    .pb_4()
                    .pt_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(
                        TextView::new(&self.detail_text)
                            .scrollable(true)
                            .selectable(true),
                    )
                    .into_any_element()
            }
            None => div()
                .flex_none()
                .px_4()
                .pb_4()
                .pt_2()
                .border_t_1()
                .border_color(cx.theme().border)
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("(no description)")
                .into_any_element(),
        };

        container.child(header).child(body)
    }

    /// Self-managed comments overlay (View comments). Rendered over the panes when
    /// `comments_open`; repaints when `CommentsLoaded` arrives. Keys are scoped to
    /// `COMMENTS_CONTEXT` via the focused panel.
    fn render_comments_overlay(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let hidden_count = self.comments.iter().filter(|c| c.is_minimized).count();
        let sort_label = if self.comments_sort_desc {
            "newest"
        } else {
            "oldest"
        };

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
                    let reason = c
                        .minimized_reason
                        .clone()
                        .unwrap_or_else(|| "minimized".into());
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
                        let reason = c
                            .minimized_reason
                            .clone()
                            .unwrap_or_else(|| "minimized".into());
                        block = block.child(
                            div()
                                .text_xs()
                                // `yellow` (attention), not `accent`: accent is now
                                // a muted grey that's too dark for body text.
                                .text_color(cx.theme().yellow)
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

/// Overlay GitHub Dark (Primer "dark default") colors on gpui-component's stock
/// dark theme, which is near-black and felt too dark. Only the fields the app
/// actually reads via `cx.theme()` are overridden. Must run *after* the theme
/// is switched to dark: `Theme::change` / `apply_config` rebuild `colors` from
/// the base config and would otherwise discard these.
fn apply_github_dark_overlay(cx: &mut App) {
    let c = &mut Theme::global_mut(cx).colors;
    c.background = rgb(0x0d1117).into(); // canvas.default
    c.foreground = rgb(0xe6edf3).into(); // fg.default
    c.border = rgb(0x30363d).into(); // border.default
    c.sidebar = rgb(0x161b22).into(); // canvas.subtle (left pane)
    c.sidebar_foreground = rgb(0xe6edf3).into();
    // `accent` is gpui-component's inline-code background and our unread-badge /
    // row-tint color. A bright blue there is hard to read, so use a neutral grey
    // (GitHub neutral.muted) — links and the filter-match highlight use `link`
    // instead so they stay blue.
    c.accent = rgb(0x373e47).into();
    c.accent_foreground = rgb(0xe6edf3).into();
    c.link = rgb(0x2f81f7).into(); // accent.fg — links + filter-match highlight
    c.primary = rgb(0x2f81f7).into();
    c.muted_foreground = rgb(0x8b949e).into(); // fg.muted
    c.list_active = rgb(0x21262d).into(); // selected row
    c.list_hover = rgb(0x161b22).into(); // hovered row
    c.green = rgb(0x3fb950).into(); // success (open)
    c.red = rgb(0xf85149).into(); // danger (closed)
    c.magenta = rgb(0xa371f7).into(); // done/purple (merged)
    c.yellow = rgb(0xd29922).into(); // attention (pending review)
}

/// Build the action menu for a given `MenuKind`, reusing `dispatch_action` and the
/// index-based entry helpers. Shared by right-click and the Enter action menu.
fn populate_menu(mut menu: PopupMenu, app: &Entity<GlaucaApp>, kind: MenuKind) -> PopupMenu {
    match kind {
        MenuKind::Item(item) => {
            let item = *item;
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
            menu = app_menu_item(menu, app, "Mark all as read", move |this, _w, _cx| {
                this.mark_all_read_at(index);
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
        MenuKind::CustomActions { item, actions } => {
            let item = *item;
            for action in actions {
                let item = item.clone();
                let label = action.display_label().to_string();
                menu = app_menu_item(menu, app, label, move |this, _w, _cx| {
                    this.send(EngineCommand::RunCustomAction {
                        action: Box::new(action.clone()),
                        item: Box::new(item.clone()),
                    });
                });
            }
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

/// A `label: <chips>` row in the detail pane, where the value is a wrapping row
/// of people chips (avatar + login). Used for author / assignees / reviewers.
fn detail_people_field(
    label: &str,
    chips: impl IntoIterator<Item = impl IntoElement>,
    cx: &App,
) -> impl IntoElement {
    h_flex()
        .w_full()
        .gap_2()
        .text_sm()
        .items_start()
        .child(
            div()
                .flex_shrink_0()
                .w(px(96.))
                .text_color(cx.theme().muted_foreground)
                .child(SharedString::from(label.to_string())),
        )
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .flex_wrap()
                .gap_2()
                .children(chips),
        )
}

/// A people chip: avatar + login text, shown inline in the detail header.
fn user_chip(user: UserRef, _cx: &App) -> impl IntoElement {
    let login = SharedString::from(user.login.clone());
    h_flex()
        .gap_1()
        .items_center()
        .child(user_avatar(&user))
        .child(login)
}

/// A reviewer chip: avatar with review-state overlay + login text.
fn reviewer_chip(user: UserRef, state: ReviewState, cx: &App) -> impl IntoElement {
    let login = SharedString::from(user.login.clone());
    h_flex()
        .gap_1()
        .items_center()
        .child(reviewer_avatar(&user, state, cx))
        .child(login)
}

/// Status glyph for a list row: a GitHub-style octicon (vendored under
/// `assets/octicons`, served by [`assets::Assets`]) whose shape encodes
/// issue-vs-PR and whose color encodes the state (open=green, merged=magenta,
/// closed=red, draft=muted). gpui paints the SVG as a mask tinted by
/// `text_color`.
fn item_state_icon(item: &ItemEntry, cx: &App) -> impl IntoElement {
    let (path, color) = item_state_icon_info(item, cx);
    svg()
        .path(path)
        .size_4()
        .flex_shrink_0()
        // Nudge down so the icon centers on the first title line.
        .mt(px(2.))
        .text_color(color)
}

/// Octicon path + color for an item's state, shared by the list-row status icon
/// and the detail-header state pill.
fn item_state_icon_info(item: &ItemEntry, cx: &App) -> (&'static str, Hsla) {
    let theme = cx.theme();
    if item.kind == "pull_request" {
        if item.is_draft {
            (
                "octicons/git-pull-request-draft.svg",
                theme.muted_foreground,
            )
        } else {
            match item.state.as_str() {
                "merged" => ("octicons/git-merge.svg", theme.magenta),
                "closed" => ("octicons/git-pull-request-closed.svg", theme.red),
                _ => ("octicons/git-pull-request.svg", theme.green),
            }
        }
    } else {
        match item.state.as_str() {
            "closed" => ("octicons/issue-closed.svg", theme.red),
            _ => ("octicons/issue-opened.svg", theme.green),
        }
    }
}

/// GitHub-style state label for the detail-header state pill.
fn state_label(item: &ItemEntry) -> &'static str {
    if item.kind == "pull_request" {
        if item.is_draft {
            "Draft"
        } else {
            match item.state.as_str() {
                "merged" => "Merged",
                "closed" => "Closed",
                _ => "Open",
            }
        }
    } else {
        match item.state.as_str() {
            "closed" => "Closed",
            _ => "Open",
        }
    }
}

/// Side length of the participant avatars in the item list.
const AVATAR_PX: f32 = 24.;
/// Larger avatar shown in the left-pane header (current user).
const HEADER_AVATAR_PX: f32 = 36.;
/// Side length of the review-state badge overlaid on a reviewer avatar.
const BADGE_PX: f32 = 14.;
/// Max avatars shown per group (assignees / reviewers) before a `+N` overflow.
const AVATAR_LIMIT: usize = 5;

/// GitHub serves a 460px avatar PNG by default; downscaling that to the small
/// list avatar aliases badly (looks grainy). Ask GitHub to resize server-side
/// to roughly the displayed size (2× for HiDPI sharpness) via the `s=` param.
fn sized_avatar_url(url: &str, target_px: f32) -> String {
    let px = (target_px * 2.0) as u32;
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}s={px}")
}

/// One participant avatar: the user's GitHub avatar image, falling back to the
/// login's initials placeholder when there is no avatar URL (teams, or older
/// cache rows). `name` also drives the alt/initials text.
fn user_avatar(user: &UserRef) -> Avatar {
    let mut a = Avatar::new()
        .name(user.login.clone())
        .with_size(px(AVATAR_PX));
    if let Some(url) = &user.avatar_url {
        a = a.src(sized_avatar_url(url, AVATAR_PX));
    }
    a
}

/// Octicon, color, and tooltip text for a PR's `reviewDecision` (the raw GitHub
/// value), shown as an icon in the detail header.
fn review_decision_icon(decision: &str, cx: &App) -> (&'static str, Hsla, &'static str) {
    let t = cx.theme();
    match decision {
        "APPROVED" => ("octicons/check-circle-fill.svg", t.green, "Approved"),
        "CHANGES_REQUESTED" => ("octicons/x-circle-fill.svg", t.red, "Changes requested"),
        "REVIEW_REQUIRED" => ("octicons/clock.svg", t.yellow, "Review required"),
        _ => ("octicons/comment.svg", t.muted_foreground, "Review"),
    }
}

/// Octicon, icon color, and badge background for a reviewer's [`ReviewState`],
/// shown as a small badge overlaid on the reviewer avatar. gpui tints the SVG
/// mask with `text_color`; the badge background shows through the icon's
/// knockout — white for the filled check/x (GitHub-style), otherwise the
/// neutral background as a plain ring.
fn review_state_icon(state: ReviewState, cx: &App) -> (&'static str, Hsla, Hsla) {
    let theme = cx.theme();
    match state {
        ReviewState::Approved => ("octicons/check-circle-fill.svg", theme.green, white()),
        ReviewState::ChangesRequested => ("octicons/x-circle-fill.svg", theme.red, white()),
        ReviewState::Commented | ReviewState::Dismissed => (
            "octicons/comment.svg",
            theme.muted_foreground,
            theme.background,
        ),
        ReviewState::Pending => ("octicons/clock.svg", theme.yellow, theme.background),
    }
}

/// A `+N` label for participants beyond [`AVATAR_LIMIT`].
fn avatar_overflow(n: usize, cx: &App) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(SharedString::from(format!("+{n}")))
}

/// A reviewer avatar with its review-state octicon overlaid bottom-right
/// (relative+absolute, mirroring gpui-component's Badge pattern).
fn reviewer_avatar(user: &UserRef, state: ReviewState, cx: &App) -> impl IntoElement {
    let (icon, color, badge_bg) = review_state_icon(state, cx);
    div()
        .relative()
        .flex_shrink_0()
        .size(px(AVATAR_PX))
        .child(user_avatar(user))
        .child(
            svg()
                .path(icon)
                .size(px(BADGE_PX))
                .absolute()
                .bottom(px(-3.))
                .right(px(-3.))
                .text_color(color)
                // Fills the icon's knockout and rings the badge against the
                // avatar behind it.
                .bg(badge_bg)
                .rounded_full(),
        )
}

/// Render an item title, emphasising the inline-filter match range if any.
///
/// Uses `StyledText` (not flex spans) so the title wraps across lines and the
/// row grows to fit — the highlight is an overlaid style range on the wrapping
/// text rather than a separate box that can't break mid-word.
fn highlight_title(title: &str, range: Option<(usize, usize)>, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let mut text = StyledText::new(SharedString::from(title.to_string()));
    if let Some((start, end)) = range
        && start < end
        && end <= title.len()
    {
        text = text.with_highlights([(
            start..end,
            HighlightStyle {
                // `link` (not `accent`) so the match stays a visible blue —
                // `accent` is the muted grey used for inline code / badges.
                background_color: Some(theme.link),
                color: Some(theme.accent_foreground),
                ..Default::default()
            },
        )]);
    }
    div()
        .flex_1()
        .min_w_0()
        .font_bold()
        .text_color(theme.foreground)
        .child(text)
}

/// Rows shown in the Help → Keyboard shortcuts dialog (`key`, `description`). An
/// empty description marks a section-header row. Kept in sync by hand with the
/// `KeyBinding::new(...)` table registered in `main()`.
const SHORTCUTS: &[(&str, &str)] = &[
    ("j / k  ·  ↓ / ↑", "Move cursor down / up"),
    ("h / l  ·  ← / →", "Focus previous / next pane"),
    ("Enter", "Activate (commit selection / item action menu)"),
    ("/", "Focus the filter input"),
    ("Esc", "Cancel / close overlay / leave filter"),
    ("n", "New query"),
    ("f", "New filter stream"),
    ("e", "Edit selected entry"),
    ("d", "Delete selected entry"),
    ("Shift+J / Shift+K", "Reorder selected entry down / up"),
    ("o", "Open selected item in browser"),
    ("c", "View comments for selected item"),
    ("y", "Copy selected item URL to clipboard"),
    ("x", "Run a custom action on selected item"),
    ("r", "Refresh selected list (left pane) or item"),
    ("q", "Quit"),
    ("Comments overlay", ""),
    ("j / k  ·  ↓ / ↑", "Scroll comments"),
    ("g / Shift+G", "Jump to top / bottom"),
    ("s", "Toggle sort order"),
    ("h", "Show / hide minimized comments"),
    ("q / Esc", "Close comments"),
];

impl GlaucaApp {
    /// Top dropdown menu bar. Deliberately minimal — item/entry actions stay on the
    /// keyboard and right-click menus, not here. Only app-level commands: a Glauca
    /// (app) menu and a Help menu.
    fn render_menu_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let app = cx.entity();

        // Glauca (app) menu: re-sync the selected query, then quit.
        let glauca_app = app.clone();
        let glauca_menu = Button::new("menu-glauca")
            .small()
            .ghost()
            .label("Glauca")
            .dropdown_menu(move |menu, _w, _cx| {
                let mut menu = app_menu_item(menu, &glauca_app, "Sync now", |this, _w, cx| {
                    this.select_current_entry(true);
                    cx.notify();
                });
                menu = app_menu_item(menu, &glauca_app, "Full resync", |this, _w, cx| {
                    this.full_resync_current();
                    cx.notify();
                });
                menu = menu.separator();
                app_menu_item(menu, &glauca_app, "Quit", |this, w, cx| {
                    this.on_quit(&Quit, w, cx)
                })
            });

        // View menu: theme selection (System / Light / Dark). The active choice
        // is marked with a leading check; the rest are blank-padded to align.
        let view_app = app.clone();
        let current_theme = self.theme_pref;
        let notifications_enabled = self.notifications_enabled;
        let theme_label = move |pref: ThemePreference, text: &str| {
            let mark = if pref == current_theme { "✓ " } else { "   " };
            format!("{mark}{text}")
        };
        let view_menu = Button::new("menu-view")
            .small()
            .ghost()
            .label("View")
            .dropdown_menu(move |menu, _w, _cx| {
                let menu = app_menu_item(
                    menu,
                    &view_app,
                    theme_label(ThemePreference::System, "Theme: System"),
                    |this, w, cx| this.set_theme(ThemePreference::System, w, cx),
                );
                let menu = app_menu_item(
                    menu,
                    &view_app,
                    theme_label(ThemePreference::Light, "Theme: Light"),
                    |this, w, cx| this.set_theme(ThemePreference::Light, w, cx),
                );
                let menu = app_menu_item(
                    menu,
                    &view_app,
                    theme_label(ThemePreference::Dark, "Theme: Dark"),
                    |this, w, cx| this.set_theme(ThemePreference::Dark, w, cx),
                );
                let menu = menu.separator();
                let notif_mark = if notifications_enabled { "✓ " } else { "   " };
                app_menu_item(
                    menu,
                    &view_app,
                    format!("{notif_mark}Desktop notifications"),
                    |this, _w, cx| this.toggle_notifications(cx),
                )
            });

        // Help menu: About (version) and a keyboard-shortcuts reference.
        let help_app = app.clone();
        let help_menu = Button::new("menu-help")
            .small()
            .ghost()
            .label("Help")
            .dropdown_menu(move |menu, _w, _cx| {
                let mut menu = app_menu_item(menu, &help_app, "About", |this, w, cx| {
                    this.open_about_dialog(w, cx)
                });
                menu = app_menu_item(menu, &help_app, "Keyboard shortcuts", |this, w, cx| {
                    this.open_shortcuts_dialog(w, cx)
                });
                menu
            });

        h_flex()
            .w_full()
            .flex_shrink_0()
            .px_2()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(glauca_menu)
            .child(view_menu)
            .child(help_menu)
    }

    /// Help → About: a small informational dialog with the app version. Uses the
    /// same `window.open_dialog` pattern as `open_query_form`; the default OK button
    /// dismisses it.
    fn open_about_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.open_dialog(cx, move |dlg, _w, _cx| {
            dlg.title("About Glauca")
                .w(px(360.))
                .content(move |content, _w, _cx| {
                    content
                        .gap_1()
                        .text_sm()
                        .child(SharedString::from(format!(
                            "glauca-gui {}",
                            env!("CARGO_PKG_VERSION")
                        )))
                        .child(SharedString::from(
                            "GitHub PR/issue triage for the terminal.",
                        ))
                })
        });
    }

    /// Help → Keyboard shortcuts: a read-only two-column list of the key bindings.
    fn open_shortcuts_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.open_dialog(cx, move |dlg, _w, _cx| {
            dlg.title("Keyboard shortcuts")
                .w(px(480.))
                .content(move |content, _w, _cx| {
                    let mut list = content.gap_1().text_sm();
                    for (key, desc) in SHORTCUTS {
                        if desc.is_empty() {
                            // Section header.
                            list = list
                                .child(div().pt_2().font_bold().child(SharedString::from(*key)));
                        } else {
                            list = list.child(
                                h_flex()
                                    .w_full()
                                    .gap_3()
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .w(px(160.))
                                            .child(SharedString::from(*key)),
                                    )
                                    .child(
                                        div().flex_1().min_w_0().child(SharedString::from(*desc)),
                                    ),
                            );
                        }
                    }
                    list
                })
        });
    }
}

impl Render for GlaucaApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            .on_action(cx.listener(Self::on_copy_url))
            .on_action(cx.listener(Self::on_run_custom_action))
            .on_action(cx.listener(Self::on_refresh))
            .on_action(cx.listener(Self::on_open_comments))
            .on_action(cx.listener(Self::on_comments_scroll_down))
            .on_action(cx.listener(Self::on_comments_scroll_up))
            .on_action(cx.listener(Self::on_comments_top))
            .on_action(cx.listener(Self::on_comments_bottom))
            .on_action(cx.listener(Self::on_comments_toggle_sort))
            .on_action(cx.listener(Self::on_comments_toggle_hidden))
            .on_action(cx.listener(Self::on_comments_close))
            .on_action(cx.listener(Self::on_set_theme_system))
            .on_action(cx.listener(Self::on_set_theme_light))
            .on_action(cx.listener(Self::on_set_theme_dark))
            .on_action(cx.listener(Self::on_toggle_notifications))
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_menu_bar(cx))
            .child(
                // Drag-resizable 3-pane row. The group container is `size_full`,
                // so it's wrapped in a `flex_1`/`min_h_0` div to take the height
                // left under the menu bar. The left/right panes carry explicit
                // (persisted) widths and MUST be `.flex_none()` — otherwise their
                // internal `flex_grow: 1` makes them absorb space during a drag,
                // so resizing the center/right divider would visibly stretch the
                // *left* pane instead. Only the center stays flexible (it soaks
                // up whatever the sized panels don't take). Every drag persists
                // all sizes via `on_resize`.
                div().w_full().flex_1().min_h_0().child(
                    h_resizable("panes")
                        .with_state(&self.pane_state)
                        .on_resize(|state, _window, cx| {
                            // Read-modify-write so persisting pane sizes doesn't
                            // clobber the saved theme (and vice-versa).
                            let mut settings = GuiSettings::load();
                            settings.pane_sizes = state
                                .read(cx)
                                .sizes()
                                .iter()
                                .map(|p| f32::from(*p))
                                .collect();
                            settings.save();
                        })
                        .child(
                            resizable_panel()
                                .size(px(self.pane_sizes.first().copied().unwrap_or(280.)))
                                .size_range(px(250.)..px(560.))
                                .flex_none()
                                .child(pane_frame(
                                    self.focus == Focus::QueryList,
                                    self.render_left(cx),
                                    cx,
                                )),
                        )
                        .child(
                            resizable_panel().size_range(px(250.)..px(1000.)).child(
                                pane_frame(
                                    self.focus == Focus::ItemList,
                                    self.render_center(cx),
                                    cx,
                                )
                                // The center pane is the flexible one; allow it to
                                // shrink below its content width as panels resize.
                                .min_w_0(),
                            ),
                        )
                        .child(
                            resizable_panel()
                                .size(px(self.pane_sizes.get(2).copied().unwrap_or(440.)))
                                .size_range(px(300.)..px(2400.))
                                .flex_none()
                                .child(pane_frame(
                                    self.focus == Focus::ItemDetail,
                                    self.render_detail(cx),
                                    cx,
                                )),
                        ),
                ),
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
    // Keep the guard alive for the whole program so buffered logs are flushed on
    // exit. Logs go to a file under the data dir (shared with the TUI).
    let _log_guard = glauca_core::logging::init("glauca-gui", "glauca_core=info,glauca_gui=info");
    tracing::info!("glauca-gui starting");

    // rustls needs a process-level CryptoProvider, but with both aws-lc-rs and
    // ring in the dependency graph it can't auto-select one. Install ring before
    // any TLS use (the avatar HTTP client). Ignore the error if already set.
    let _ = rustls::crypto::ring::default_provider().install_default();

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
        Engine::start(pool, gh, GuiSettings::load().sync_interval_secs).await
    })?;

    gpui_platform::application()
        .with_assets(assets::Assets)
        .run(move |cx| {
            // gpui defaults to a NullHttpClient, which can't fetch the remote
            // GitHub avatar URLs the item-list avatars use (every fetch fails and
            // the Avatar falls back to its placeholder). Install a real client.
            match reqwest_client::ReqwestClient::user_agent("glauca-gui") {
                Ok(client) => cx.set_http_client(std::sync::Arc::new(client)),
                Err(e) => tracing::warn!(error = %e, "failed to init HTTP client for avatars"),
            }
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
                KeyBinding::new("y", CopyUrl, Some(NAV_CONTEXT)),
                KeyBinding::new("x", RunCustomAction, Some(NAV_CONTEXT)),
                KeyBinding::new("r", Refresh, Some(NAV_CONTEXT)),
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
