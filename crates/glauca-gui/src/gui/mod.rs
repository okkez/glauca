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

use glauca_core::actions::{CustomAction, CustomActions};
use glauca_core::engine::{Engine, EngineCommand, EngineInit, ReviewEvent};
use glauca_core::filter::FilterQuery;
use glauca_core::logic::{expand_me, reviewer_overlays};
use glauca_core::notify::ItemTracker;
use glauca_core::types::{CommentEntry, ItemAction, ItemEntry, LeftPaneEntry};
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::avatar::Avatar;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu, PopupMenu, PopupMenuItem};
use gpui_component::notification::Notification;
use gpui_component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_component::scroll::ScrollableElement;
use gpui_component::text::{TextView, TextViewState, markdown};
use gpui_component::tooltip::Tooltip;
use gpui_component::{
    ActiveTheme, Root, Sizable, StyledExt, Theme, ThemeMode, WindowExt, h_flex, v_flex,
};
use smol::Timer;
use tokio::sync::mpsc::Sender;

mod assets;
mod actions;
mod dialogs;
mod forms;
mod entries;
mod comments;
mod menu;
mod message;
mod run;
mod scroll;
mod settings;
mod widgets;
pub(crate) use menu::app_menu_item;
pub(crate) use run::run;
pub(crate) use scroll::{DETAIL_SCROLL_STEP, pane_frame, scroll_vertically};
use settings::{GuiSettings, ThemePreference};
pub(crate) use widgets::{
    AVATAR_LIMIT, HEADER_AVATAR_PX, apply_github_dark_overlay, avatar_overflow, detail_field,
    detail_people_field, highlight_title, item_state_icon, item_state_icon_info,
    review_decision_icon, reviewer_avatar, reviewer_chip, sized_avatar_url, state_label,
    user_avatar, user_chip,
};

/// How often the GUI drains engine messages and repaints.
const DRAIN_INTERVAL: Duration = Duration::from_millis(50);

/// Idle delay before a filter keystroke triggers a re-filter, so typing fast in a
/// large list doesn't recompute on every character.
const FILTER_DEBOUNCE: Duration = Duration::from_millis(150);

/// Idle delay before in-memory settings are flushed to `gui.toml`, so a pane
/// drag (which fires `on_resize` per mouse move) writes once at the end instead
/// of doing disk I/O on the UI thread for every event. `on_quit` flushes
/// synchronously, so a quit right after a change still persists it.
const SETTINGS_SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

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
pub(crate) enum Focus {
    QueryList,
    ItemList,
    ItemDetail,
}

/// What a context/action menu operates on (see `open_menu` / `populate_menu`).
pub(crate) enum MenuKind {
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
pub(crate) struct FilterStreamFormParams {
    edit: Option<i64>,
    parent_id: i64,
    kind: String,
    init_name: String,
    init_filter: String,
}

pub(crate) struct GlaucaApp {
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
    /// Drag-resizable left/center/right pane widths. Mirrored into
    /// `settings.pane_sizes` on every resize and restored on startup.
    pane_state: Entity<ResizableState>,
    /// In-memory settings — the single source of truth while the app runs.
    /// Loaded once in `main` and only ever written back from here (via
    /// `schedule_settings_save` / the `on_quit` flush), so persisting one field
    /// can never clobber another with stale on-disk state.
    settings: GuiSettings,
    /// Pending debounced settings flush; replacing it cancels the previous one
    /// (same pattern as `filter_task`).
    settings_save_task: Option<Task<()>>,
    /// Parsed state for the detail pane's Markdown body. Held as an entity so the
    /// parse is retained across frames. Content is synced from the selected item
    /// in `render_detail` (a no-op when unchanged).
    detail_text: Entity<TextViewState>,
    /// Scroll position of the detail body. The body is a tracked
    /// `overflow_y_scroll` container (same pattern as the comments overlay)
    /// rather than `TextView::scrollable`: the TextView's internal ListState is
    /// private to gpui-component, so keyboard scrolling (j/k in `ItemDetail`)
    /// needs a handle we own. Reset to the top whenever the shown item changes,
    /// mirroring the TUI's `detail_scroll = 0`.
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

    /// Selected event for the open review dialog (Comment / Approve / Request
    /// Changes); reset to `Approve` each time the dialog opens.
    review_action: ReviewEvent,

    /// Inline filter input. Its `Change` events update `filter` (see `new`).
    filter_input: Entity<InputState>,
    /// Pending debounced re-filter task; replacing it cancels the previous one.
    filter_task: Option<Task<()>>,
    /// Per-query session baseline for the notification "N updated" count, so the
    /// first load of each query establishes a baseline without notifying (no
    /// startup storm). See `glauca_core::notify::ItemTracker`.
    notif_tracker: ItemTracker,
    /// User-defined custom actions loaded from `actions.toml` (see
    /// `glauca_core::actions`). Offered via the `x` picker and the item menu's
    /// "Custom actions" submenu, filtered by kind.
    custom_actions: CustomActions,
    /// Keeps the `filter_input` subscription alive for the view's lifetime.
    _subscriptions: Vec<Subscription>,
}

impl GlaucaApp {
    fn new(
        engine: Engine,
        init: EngineInit,
        settings: GuiSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Periodically drain engine messages and repaint while the window lives.
        // Use gpui's executor timer (not `smol::Timer`): its completion wakes the
        // platform event loop, so the loop keeps draining while the window is idle.
        // With `smol::Timer` the tick doesn't poke gpui's loop, so background-sync
        // messages sat undrained until the next user interaction (no "N updated"
        // banner appeared on its own).
        // `spawn_in` (not `spawn`) so `apply` gets a `&mut Window`: error
        // messages surface as `push_notification` toasts, which need the window.
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(DRAIN_INTERVAL).await;
                let result = this.update_in(cx, |this, window, cx| {
                    let mut changed = false;
                    while let Some(msg) = this.engine.try_recv() {
                        this.apply(msg, window, cx);
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
                            this.reset_detail_scroll();
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
            settings,
            settings_save_task: None,
            detail_text,
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
            review_action: ReviewEvent::Approve,
            filter_input,
            filter_task: None,
            notif_tracker: ItemTracker::new(),
            custom_actions: CustomActions::load(),
            _subscriptions: vec![subscription],
        };
        // Apply the saved theme up front (System follows the OS appearance).
        app.apply_theme(Some(window), cx);
        // While following the OS, re-sync whenever its appearance flips. The
        // closure re-reads the theme setting so pinning Light/Dark stops the follow.
        let this = cx.entity();
        let appearance_sub = window.observe_window_appearance(move |window, cx| {
            this.update(cx, |app, cx| {
                if app.settings.theme == ThemePreference::System {
                    // Re-apply via `apply_theme` so the GitHub dark overlay is
                    // re-applied when the OS flips to dark.
                    app.apply_theme(Some(window), cx);
                }
            });
        });
        app._subscriptions.push(appearance_sub);
        // Flush pending settings whenever the app quits. Every quit trigger funnels
        // through `cx.quit()` → `shutdown()`, which runs quit observers synchronously
        // before dropping the entity, so the write always completes. On non-macOS,
        // closing the last window quits the app, so an OS-initiated close (title-bar
        // ×, Alt-F4) reaches this hook too; the `q`/menu Quit action reaches it via
        // `cx.quit()`. (On macOS the sole window closing leaves the app running with
        // settings still in memory — the eventual Cmd-Q flushes them.) Without this
        // hook, a change made inside the debounce window right before an OS-initiated
        // quit would be lost — a regression from the old eager per-event save.
        let quit_sub = cx.on_app_quit(|app, _cx| {
            // Cancel the still-pending debounce first so it can't race this write,
            // then flush once synchronously.
            app.settings_save_task = None;
            app.settings.save();
            async {}
        });
        app._subscriptions.push(quit_sub);
        app.prime();
        // Restore saved column widths into the authoritative ResizableState after
        // the first frame is drawn (panels are synced and the container has a real
        // size by then). The `.size()` initial_size hints on the panels lose to
        // `adjust_to_container_size`, which overwrites `panel.size` on that first
        // prepaint — so seed the widths explicitly here. `on_next_frame` is a
        // one-shot, so no guard flag is needed.
        let pane_state = app.pane_state.clone();
        let left = app.settings.pane_sizes.first().copied();
        let right = app.settings.pane_sizes.get(2).copied();
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

    /// Apply `self.settings.theme` to the global gpui-component theme. `System`
    /// follows the OS appearance; `Light`/`Dark` pin an explicit mode. When the
    /// resolved mode is dark, overlay the GitHub-flavored palette (the stock
    /// dark theme is near-black) — see `apply_github_dark_overlay`.
    fn apply_theme(&self, window: Option<&mut Window>, cx: &mut App) {
        match self.settings.theme {
            ThemePreference::System => Theme::sync_system_appearance(window, cx),
            ThemePreference::Light => Theme::change(ThemeMode::Light, window, cx),
            ThemePreference::Dark => Theme::change(ThemeMode::Dark, window, cx),
        }
        if cx.theme().mode.is_dark() {
            apply_github_dark_overlay(cx);
        }
    }

    /// Flush the in-memory settings to disk after a short idle delay, off the UI
    /// thread. Replacing the task cancels a still-pending flush (same pattern as
    /// `filter_task`), so a burst of changes — a pane drag most of all — writes
    /// once. The `on_app_quit` hook flushes synchronously so a change made inside
    /// the debounce window right before quitting isn't lost.
    fn schedule_settings_save(&mut self, cx: &mut Context<Self>) {
        self.settings_save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SETTINGS_SAVE_DEBOUNCE).await;
            let Ok(settings) = this.update(cx, |this, _| this.settings.clone()) else {
                return; // entity gone; the on_app_quit hook already flushed on quit
            };
            cx.background_executor()
                .spawn(async move { settings.save() })
                .await;
        }));
    }

    /// Switch the theme from the View menu: apply it, schedule a save, repaint.
    fn set_theme(&mut self, pref: ThemePreference, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.theme = pref;
        self.apply_theme(Some(window), cx);
        self.schedule_settings_save(cx);
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

    /// Toggle desktop notifications from the View menu: flip the flag, schedule
    /// a save, and repaint the menu marker.
    fn toggle_notifications(&mut self, cx: &mut Context<Self>) {
        self.settings.notifications_enabled = !self.settings.notifications_enabled;
        self.schedule_settings_save(cx);
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
        self.reset_detail_scroll();
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
        self.reset_detail_scroll();
        self.stream_filter = stream_filter;
        self.clear_pending();
        self.recompute_filtered();
        self.send(EngineCommand::LoadCached { query_id: root_id });
    }

    /// Scroll the detail body back to the top. Called whenever the shown item
    /// changes (cursor move / entry switch / re-filter), mirroring the TUI's
    /// `detail_scroll = 0` reset.
    fn reset_detail_scroll(&self) {
        self.detail_scroll.set_offset(point(px(0.), px(0.)));
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
                    self.reset_detail_scroll();
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
                    self.items_list.scroll_to_reveal_item(self.item_cursor);
                    self.reset_detail_scroll();
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

    // ── Comments overlay keys (scoped to COMMENTS_CONTEXT) ───────────────────────

    // ── Entry add / edit dialogs ───────────────────────────────────────────────

    // ── Item actions ───────────────────────────────────────────────────────────

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
        let title_el = highlight_title(&item.title, fq.highlight_ranges(&item.title), cx);

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

        // Body rendered as Markdown via gpui-component's `TextView`, inside a
        // tracked `overflow_y_scroll` container (the comments-overlay pattern)
        // instead of the TextView's own virtualized `scrollable(true)` mode: that
        // mode's ListState is private to gpui-component, which would leave the
        // j/k keyboard scroll (Focus::ItemDetail) with nothing to drive. The
        // Markdown parse is still retained in `detail_text` (`set_text` is a
        // no-op unless the selected item's body actually changed); only layout
        // re-runs on pane resize.
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
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .relative()
                    .child(
                        div()
                            .id("detail-scroll")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.detail_scroll)
                            .px_4()
                            .pb_4()
                            .pt_2()
                            .child(TextView::new(&self.detail_text).selectable(true)),
                    )
                    .vertical_scrollbar(&self.detail_scroll)
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
        let current_theme = self.settings.theme;
        let notifications_enabled = self.settings.notifications_enabled;
        let theme_label = move |pref: ThemePreference, text: &str| {
            let mark = if pref == current_theme { "✓ " } else { "   " };
            format!("{mark}{text}")
        };
        let view_menu = Button::new("menu-view")
            .small()
            .ghost()
            .label("View")
            .dropdown_menu(move |menu, _w, _cx| {
                let menu = [
                    (ThemePreference::System, "Theme: System"),
                    (ThemePreference::Light, "Theme: Light"),
                    (ThemePreference::Dark, "Theme: Dark"),
                ]
                .into_iter()
                .fold(menu, |menu, (pref, text)| {
                    app_menu_item(
                        menu,
                        &view_app,
                        theme_label(pref, text),
                        move |this, w, cx| this.set_theme(pref, w, cx),
                    )
                });
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
                        .on_resize({
                            // Mirror the drag into the in-memory settings (the
                            // single source of truth) and let the debounced task
                            // flush once the drag pauses — no disk I/O per event.
                            let this = cx.entity().downgrade();
                            move |state, _window, cx| {
                                let sizes: Vec<f32> = state
                                    .read(cx)
                                    .sizes()
                                    .iter()
                                    .map(|p| f32::from(*p))
                                    .collect();
                                let _ = this.update(cx, |app, cx| {
                                    app.settings.pane_sizes = sizes;
                                    app.schedule_settings_save(cx);
                                });
                            }
                        })
                        .child(
                            resizable_panel()
                                .size(px(self
                                    .settings
                                    .pane_sizes
                                    .first()
                                    .copied()
                                    .unwrap_or(280.)))
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
                                .size(px(self.settings.pane_sizes.get(2).copied().unwrap_or(440.)))
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
