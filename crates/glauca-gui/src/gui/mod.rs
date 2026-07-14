//! glauca-gui — gpui front-end for glauca (phase B, MVP 閲覧先行).
//!
//! gpui owns the main-thread event loop and is not tokio-aware, so the async
//! engine runs on a separate multi-thread tokio runtime. A foreground task
//! awaits the engine's message receiver and repaints per batch (see
//! `setup.rs`); commands are sent from non-async click handlers via a cloned
//! `EngineCommand` sender.
//!
//! B1: two panes. Left = the left-pane entries (root queries + indented filter
//! streams) as a clickable list with selection highlight and unread badges.
//! Center = the cached item list for the selected entry, with `NEW` badges and
//! scrolling. Selecting an entry mirrors the TUI `select_current_entry` flow:
//! load cached items, mark the entry viewed, and (for root queries) sync.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use glauca_core::actions::{CustomAction, CustomActions};
use glauca_core::engine::{EngineCommand, ReviewEvent};
use glauca_core::filter::FilterQuery;
use glauca_core::logic::reviewer_overlays;
use glauca_core::notify::ItemTracker;
use glauca_core::types::{CommentEntry, ItemAction, ItemEntry, LeftPaneEntry};
use gpui::*;
use gpui_component::avatar::Avatar;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputState};
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use gpui_component::notification::Notification;
use gpui_component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_component::text::TextViewState;
use gpui_component::{StyledExt, WindowExt, h_flex};
use tokio::sync::mpsc::Sender;

mod actions;
mod assets;
mod comments;
mod detail;
mod dialogs;
mod entries;
mod forms;
mod menu;
mod message;
mod nav;
mod render;
mod run;
mod scroll;
mod settings;
mod setup;
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
    /// Cloneable command sender, used from non-async click handlers. The
    /// engine itself (with the message receiver) is moved into the push-based
    /// delivery loop spawned by `new` (see `setup.rs`), not held on the view.
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
    /// Item keys `(repo_owner, repo_name, number)` whose body re-fetch was already
    /// requested this session. Cache maintenance clears the re-fetchable `body` of
    /// old items; when such an item is viewed we fetch it once on demand, and this
    /// set prevents re-dispatching on repeated selection.
    body_refresh_requested: HashSet<(String, String, i64)>,

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

    /// When the previous frame's element tree was built. Drives the `frame`
    /// debug log (gap between rebuilds) used to diagnose repaint backlogs;
    /// costs one Instant per frame when debug logging is off.
    last_render_at: Option<std::time::Instant>,
}
