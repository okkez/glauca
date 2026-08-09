use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use octocrab::Octocrab;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use sqlx::SqlitePool;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub mod icons;
mod keys;
mod message;
mod mouse;
mod process;
mod run;
mod select;
pub mod settings;
pub mod single_line_input;
mod state;
mod terminal;
pub mod ui;

#[cfg(test)]
mod test_support;

use icons::Icons;
use keys::handle_key;
use message::handle_app_message;
use mouse::{MouseRegions, MouseTarget, handle_mouse};
pub(crate) use process::{copy_to_clipboard_osc52, item_actions, run_editor, run_octorus_review};
pub(crate) use run::run;
use select::{
    full_resync_selected, mark_selected_item_read, refetch_selected_body_if_missing,
    refresh_selected_item, refresh_selected_list, reorder_command, select_current_entry,
};
use single_line_input::SingleLineInput;
pub(crate) use state::{
    active_filter_stream_field_mut, clear_active_modal_field, modal_fields, modal_fields_ref,
    sync_modal_cursors,
};
pub(crate) use terminal::{enter_tui, leave_tui, reenter_tui};

use glauca_core::actions::{CustomAction, CustomActions};
use glauca_core::engine::{AppMessage, Engine, EngineCommand, ReviewEvent};
use glauca_core::filter::FilterQuery;
use glauca_core::logic::{ChangeCounts, group_range, is_item_unread, query_label};
use glauca_core::notify::ItemTracker;
use settings::TuiSettings;

// glauca-core::types を従来名で使えるよう re-export。
pub use glauca_core::types::{
    CommentEntry, EntryKey, ItemAction, ItemEntry, LeftPaneEntry, MergeStrategy, QueryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Focus {
    QueryList,
    ItemList,
    ItemDetail,
}

#[derive(Debug, PartialEq)]
pub enum InputMode {
    Normal,
    Filter,
    NewQuery,
    /// New filter stream modal (name + filter, Tab to switch fields).
    NewFilterStream,
    /// Edit root query modal (display name + GitHub search query, Tab to switch fields).
    EditQuery,
    /// Edit filter stream modal (name + filter, Tab to switch fields).
    EditFilterStream,
    ActionMenu,
    /// User-defined custom-action picker (opened with `x`).
    CustomActionMenu,
    MergeMenu,
    /// Review-event selection (Comment / Approve / Request changes), shown after
    /// the review-comment editor closes so the submit can be confirmed/cancelled.
    ReviewMenu,
    /// Comments popup (fetched via API, displayed in-TUI).
    CommentsPopup,
    /// Keybinding cheat-sheet overlay (opened with `?`).
    Help,
}

/// Memoized result of `filter_items` for the current list. `filtered_items()` is called
/// several times per render, and each call otherwise re-parses the query and fuzzy-matches
/// every item. Stores indices, not `&ItemEntry`, so it doesn't borrow `App::items`.
#[derive(Default)]
struct FilteredCache {
    /// `(items_version, stream_filter, inline_filter, current_user)` the cached
    /// `indices` were computed from. A mismatch triggers recomputation.
    key: Option<(u64, Option<String>, String, Option<String>)>,
    indices: Vec<usize>,
}

pub struct App {
    pub focus: Focus,
    pub input_mode: InputMode,

    pub entries: Vec<LeftPaneEntry>,
    pub entry_cursor: usize,

    /// The visible item list. Change it structurally only through `apply_items_to_view` /
    /// `clear_items`, which bump `items_version` to invalidate `filtered_cache`. In-place
    /// field edits like marking-read are fine — they don't affect which items match.
    pub items: Vec<ItemEntry>,
    /// Bumped whenever `items` is replaced, to invalidate `filtered_cache`.
    items_version: u64,
    /// Memoized `filter_items` indices; see [`FilteredCache`].
    filtered_cache: RefCell<FilteredCache>,
    pub item_cursor: usize,
    pub unread_counts: HashMap<EntryKey, usize>,
    /// Freshly-synced items for the currently-viewed query, held back because
    /// they came from a background sync. Applied on explicit action (`u`).
    pub pending_items: Option<Vec<ItemEntry>>,
    /// How `pending_items` differs from the displayed list (new/updated and
    /// removed), which drives the banner text.
    pub pending_changes: ChangeCounts,
    pub filter: SingleLineInput,
    /// Active filter stream filter applied before the inline filter (if any).
    pub stream_filter: Option<String>,

    pub new_query_input: SingleLineInput,
    pub new_query_name: SingleLineInput,
    /// Name field of the filter-stream create/edit modal (shared by new & edit).
    pub filter_stream_name: SingleLineInput,
    /// Filter-stream OR-group boxes, always at least one. Shared by the new and edit
    /// filter-stream modals.
    pub filter_stream_filters: Vec<SingleLineInput>,
    /// Display-name buffer for the Edit Query modal (field 0).
    pub edit_input: SingleLineInput,
    /// Search-query buffer for the Edit Query modal (field 1).
    pub edit_input2: SingleLineInput,
    /// Active field in a modal. For the 2-field query modals: 0 or 1. For the
    /// filter-stream modals: 0 = name, 1..=N = the N-th `filter_stream_filters`
    /// box (index `modal_field - 1`).
    pub modal_field: usize,
    pub action_cursor: usize,
    pub merge_strategy_cursor: usize,
    /// Selection cursor for the review-event menu (`ReviewEvent::all()` order).
    pub review_event_cursor: usize,
    /// Review comment captured from the editor, pending the review-event choice.
    pub review_body: Option<String>,
    /// Comments fetched for the comments popup.
    pub comments: Vec<CommentEntry>,
    /// True while comments are being fetched from the API.
    pub comments_loading: bool,
    /// Scroll offset within the comments popup.
    pub comments_scroll: usize,
    /// Whether to show minimized/hidden comments (default: false = collapsed).
    pub comments_show_hidden: bool,
    /// Sort order for comments: true = newest first, false = oldest first.
    pub comments_sort_desc: bool,
    pub status: Option<String>,
    /// Whether a manual GitHub sync is in progress for the selected query.
    pub syncing: bool,
    /// Number of pending background auto-refresh jobs (queued + in-progress).
    pub bg_sync_pending: usize,
    /// Scroll offset for the detail pane (right column).
    pub detail_scroll: u16,
    /// Login name of the authenticated GitHub user (used to expand `@me` in filters).
    pub current_user: Option<String>,
    /// Whether background-sync arrivals fire OS desktop notifications. Loaded
    /// from `TuiSettings`; toggled with `N`.
    pub notifications_enabled: bool,
    /// Per-query session baseline for the notification "N updated" count.
    pub notif_tracker: ItemTracker,
    /// Active semantic-icon set (emoji/Unicode vs icon-font glyphs). Loaded from
    /// `TuiSettings::use_icon_font`; toggled with `F`.
    pub icons: Icons,
    /// Custom actions from `actions.toml`, offered via the `x` picker, filtered by kind.
    pub custom_actions: CustomActions,
    /// Selection cursor within the custom-action picker (indexes the list
    /// returned by `custom_actions_for_selected`).
    pub custom_action_cursor: usize,
    /// Item keys whose body re-fetch was already requested this session. Maintenance
    /// clears the re-fetchable `body` of old items; this set stops the on-demand fetch
    /// from re-dispatching on every keypress.
    pub body_refresh_requested: HashSet<(String, String, i64)>,
    /// Last-rendered pane geometry, captured each frame by the `ui` draw
    /// functions so mouse events can hit-test coordinates back to a pane/row.
    /// Interior-mutable like `filtered_cache` so `draw` keeps its `&App`.
    pub(crate) mouse_regions: RefCell<MouseRegions>,
    /// Time and target of the last left-click, for double-click detection.
    pub(crate) last_mouse_click: Option<(Instant, MouseTarget)>,
}

// `AppMessage` / `SyncJob` は glauca_core::engine にある。

enum Action {
    None,
    Quit,
    LoadEntry,
    /// Like `LoadEntry` but only syncs when the cache is stale (no forced GitHub
    /// fetch). Used by wheel-scrolling the query pane, which can emit several
    /// notches per gesture — a forced sync each would be a burst of API calls.
    LoadEntryCached,
    SaveNewQuery,
    SaveNewFilterStream,
    SaveEditQuery,
    SaveEditFilterStream,
    Confirm,
    ConfirmMergeStrategy,
    ConfirmReviewEvent,
    OpenBrowser,
    CopyUrl,
    /// Confirm the highlighted entry in the custom-action picker.
    ConfirmCustom,
    ReviewOctorus,
    RefreshList,
    RefreshItem,
    /// Force a full re-fetch + prune of the selected query (`S`).
    FullResync,
    /// Apply held-back background-sync results to the visible list (`u`).
    ApplyPending,
}
