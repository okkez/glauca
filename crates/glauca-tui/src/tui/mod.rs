use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use octocrab::Octocrab;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use sqlx::SqlitePool;
use std::{
    cell::RefCell,
    collections::HashMap,
    io,
    time::{SystemTime, UNIX_EPOCH},
};

pub mod icons;
pub mod settings;
pub mod single_line_input;
mod state;
pub mod ui;

#[cfg(test)]
mod test_support;

use icons::Icons;
use single_line_input::SingleLineInput;
pub(crate) use state::{
    clear_active_modal_field, modal_fields, modal_fields_ref, sync_modal_cursors,
};

use glauca_core::actions::{CustomAction, CustomActions};
use glauca_core::engine::{AppMessage, Engine, EngineCommand, ReviewEvent};
use glauca_core::filter::FilterQuery;
use glauca_core::logic::{group_range, is_item_unread, move_group_down, query_label};
use glauca_core::notify::ItemTracker;
use settings::TuiSettings;

// ── Display/domain types ─────────────────────────────────────────────────────
// Moved to glauca-core::types (framework 非依存)。TUI からは従来名で使えるよう re-export。
pub use glauca_core::types::{
    CommentEntry, ItemAction, ItemEntry, LeftPaneEntry, MergeStrategy, QueryEntry,
};

// ── Application state ────────────────────────────────────────────────────────

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

/// Memoized result of `filter_items` for the current list. `filtered_items()`
/// is called several times per render (list, detail, status bar) and each call
/// otherwise re-parses the query and fuzzy-matches every item; caching the
/// matching indices collapses those to one pass while the inputs are unchanged.
/// Stores indices (not `&ItemEntry`) so it doesn't borrow `App::items`.
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

    /// The visible item list. Change it structurally only through
    /// `apply_items_to_view` / `clear_items`, which bump `items_version` to
    /// invalidate `filtered_cache`; a direct replace/reorder here would leave the
    /// memoized filter indices stale (in-place field edits like marking-read are
    /// fine, since they don't affect which items match).
    pub items: Vec<ItemEntry>,
    /// Bumped whenever `items` is replaced, to invalidate `filtered_cache`.
    items_version: u64,
    /// Memoized `filter_items` indices; see [`FilteredCache`].
    filtered_cache: RefCell<FilteredCache>,
    pub item_cursor: usize,
    pub unread_counts: HashMap<(bool, i64), usize>,
    /// Freshly-synced items for the currently-viewed query, held back because
    /// they came from a background sync. Applied on explicit action (`u`).
    pub pending_items: Option<Vec<ItemEntry>>,
    /// How many of `pending_items` are new/updated vs the displayed list.
    pub pending_count: usize,
    pub filter: SingleLineInput,
    /// Active filter stream filter applied before the inline filter (if any).
    pub stream_filter: Option<String>,

    pub new_query_input: SingleLineInput,
    pub new_query_name: SingleLineInput,
    pub new_filter_stream_name: SingleLineInput,
    pub new_filter_stream_filter: SingleLineInput,
    /// Input buffer reused for edit modals (display name or step-1 field).
    pub edit_input: SingleLineInput,
    /// Second input buffer for edit modals (query string or filter string).
    pub edit_input2: SingleLineInput,
    /// Which field (0 or 1) is active in a 2-field modal.
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
    /// Per-query session baseline for the notification "N updated" count, so the
    /// first load of each query establishes a baseline without notifying (no
    /// startup storm). See `glauca_core::notify::ItemTracker`.
    pub notif_tracker: ItemTracker,
    /// Active semantic-icon set (emoji/Unicode vs icon-font glyphs). Loaded from
    /// `TuiSettings::use_icon_font`; toggled with `F`.
    pub icons: Icons,
    /// User-defined custom actions loaded from `actions.toml` (see
    /// `glauca_core::actions`). Offered via the picker (`x`), filtered by kind.
    pub custom_actions: CustomActions,
    /// Selection cursor within the custom-action picker (indexes the list
    /// returned by `custom_actions_for_selected`).
    pub custom_action_cursor: usize,
}

// `AppMessage` / `SyncJob` は glauca_core::engine へ移設（A6）。

// ── Actions returned from key handling ───────────────────────────────────────

enum Action {
    None,
    Quit,
    LoadEntry,
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

/// Copy `text` to the system clipboard via the OSC 52 terminal escape sequence.
/// This works without a clipboard tool (xclip/pbcopy) or X11/Wayland, and over
/// SSH, as long as the terminal emulator supports OSC 52. The sequence is just
/// an escape code, so writing it mid-session does not disturb the alternate
/// screen.
fn copy_to_clipboard_osc52(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let seq = osc52_sequence(text);
    let mut out = io::stdout();
    out.write_all(seq.as_bytes())?;
    out.flush()
}

/// Build the OSC 52 clipboard escape sequence for `text` (pure; the side effect
/// of writing to the terminal lives in [`copy_to_clipboard_osc52`]).
fn osc52_sequence(text: &str) -> String {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    format!("\x1b]52;c;{}\x07", STANDARD.encode(text.as_bytes()))
}

// ── Key event handler ────────────────────────────────────────────────────────

fn handle_key(app: &mut App, key: KeyEvent) -> Action {
    match app.input_mode {
        InputMode::Filter => handle_key_filter(app, key),
        InputMode::NewQuery => handle_key_new_query(app, key),
        InputMode::NewFilterStream => handle_key_new_filter_stream(app, key),
        InputMode::EditQuery => handle_key_edit_query(app, key),
        InputMode::EditFilterStream => handle_key_edit_filter_stream(app, key),
        InputMode::ActionMenu => handle_key_action_menu(app, key),
        InputMode::CustomActionMenu => handle_key_custom_action_menu(app, key),
        InputMode::MergeMenu => handle_key_merge_menu(app, key),
        InputMode::ReviewMenu => handle_key_review_menu(app, key),
        InputMode::CommentsPopup => handle_key_comments_popup(app, key),
        InputMode::Help => handle_key_help(app, key),
        InputMode::Normal => handle_key_normal(app, key),
    }
}

/// Keybinding overlay: Esc / `?` / `q` close it; everything else is ignored.
fn handle_key_help(app: &mut App, key: KeyEvent) -> Action {
    if matches!(
        key.code,
        KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q')
    ) {
        app.input_mode = InputMode::Normal;
    }
    Action::None
}

fn handle_key_normal(app: &mut App, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('q') => return Action::Quit,

        // Show the keybinding cheat-sheet overlay (works from any pane).
        KeyCode::Char('?') => {
            app.input_mode = InputMode::Help;
        }

        // Focus cycling — h/l or left/right arrows
        KeyCode::Char('l') | KeyCode::Right => {
            app.focus = match app.focus {
                Focus::QueryList => Focus::ItemList,
                Focus::ItemList => Focus::ItemDetail,
                Focus::ItemDetail => Focus::QueryList,
            };
        }
        KeyCode::Char('h') | KeyCode::Left => {
            app.focus = match app.focus {
                Focus::QueryList => Focus::ItemDetail,
                Focus::ItemList => Focus::QueryList,
                Focus::ItemDetail => Focus::ItemList,
            };
        }

        // Navigation
        KeyCode::Char('j') | KeyCode::Down => match app.focus {
            Focus::QueryList => {
                if app.entry_cursor + 1 < app.entries.len() {
                    app.entry_cursor += 1;
                    return Action::LoadEntry;
                }
            }
            Focus::ItemList => {
                let max = app.filtered_items().len().saturating_sub(1);
                if app.item_cursor < max {
                    app.item_cursor += 1;
                    app.detail_scroll = 0;
                }
            }
            Focus::ItemDetail => {
                app.detail_scroll = app.detail_scroll.saturating_add(1);
            }
        },
        KeyCode::Char('k') | KeyCode::Up => match app.focus {
            Focus::QueryList => {
                if app.entry_cursor > 0 {
                    app.entry_cursor -= 1;
                    return Action::LoadEntry;
                }
            }
            Focus::ItemList => {
                if app.item_cursor > 0 {
                    app.item_cursor -= 1;
                    app.detail_scroll = 0;
                }
            }
            Focus::ItemDetail => {
                app.detail_scroll = app.detail_scroll.saturating_sub(1);
            }
        },

        // New root query (left pane)
        KeyCode::Char('n') if app.focus == Focus::QueryList => {
            app.input_mode = InputMode::NewQuery;
            app.modal_field = 0;
            app.new_query_name = SingleLineInput::new();
            app.new_query_input = SingleLineInput::new();
        }
        // New filter stream (left pane) — only when a root query or filter stream is selected
        KeyCode::Char('f') if app.focus == Focus::QueryList && !app.entries.is_empty() => {
            app.input_mode = InputMode::NewFilterStream;
            app.modal_field = 0;
            app.new_filter_stream_name = SingleLineInput::new();
            app.new_filter_stream_filter = SingleLineInput::new();
        }
        // Edit selected entry (left pane)
        KeyCode::Char('e') if app.focus == Focus::QueryList => {
            if let Some(entry) = app.entries.get(app.entry_cursor) {
                match entry {
                    LeftPaneEntry::Query(q) => {
                        app.edit_input = SingleLineInput::from_text(q.label.clone());
                        app.edit_input2 = SingleLineInput::from_text(q.query_str.clone());
                        app.modal_field = 0;
                        app.input_mode = InputMode::EditQuery;
                    }
                    LeftPaneEntry::FilterStream(fs) => {
                        app.edit_input = SingleLineInput::from_text(fs.name.clone());
                        app.edit_input2 = SingleLineInput::from_text(fs.filter.clone());
                        app.modal_field = 0;
                        app.input_mode = InputMode::EditFilterStream;
                    }
                }
            }
        }
        // Deletion sends an async engine command, which this sync handler can't
        // do, so it's handled in the main loop. Swallow 'd' here so Normal-mode
        // default handling doesn't also run.
        KeyCode::Char('d')
            if app.focus == Focus::QueryList && key.modifiers.contains(KeyModifiers::NONE) => {}

        KeyCode::Enter
            if matches!(app.focus, Focus::ItemList | Focus::ItemDetail)
                && app.selected_item().is_some() =>
        {
            app.input_mode = InputMode::ActionMenu;
            app.action_cursor = 0;
        }

        // Open selected item in browser directly
        KeyCode::Char('o')
            if matches!(app.focus, Focus::ItemList | Focus::ItemDetail)
                && app.selected_item().is_some() =>
        {
            return Action::OpenBrowser;
        }

        // Copy selected item URL to the clipboard directly
        KeyCode::Char('y')
            if matches!(app.focus, Focus::ItemList | Focus::ItemDetail)
                && app.selected_item().is_some() =>
        {
            return Action::CopyUrl;
        }

        // Open the custom-action picker for the selected item (`x`). No-op with a
        // hint when no defined action applies to this item's kind.
        KeyCode::Char('x')
            if matches!(app.focus, Focus::ItemList | Focus::ItemDetail)
                && app.selected_item().is_some() =>
        {
            if !app.has_custom_actions_for_selected() {
                app.status = Some("No custom actions for this item".into());
            } else {
                app.input_mode = InputMode::CustomActionMenu;
                app.custom_action_cursor = 0;
            }
        }

        // Review the selected PR with octorus (`or`). PR-only.
        KeyCode::Char('R')
            if matches!(app.focus, Focus::ItemList | Focus::ItemDetail)
                && app
                    .selected_item()
                    .map(|i| i.kind == "pull_request")
                    .unwrap_or(false) =>
        {
            return Action::ReviewOctorus;
        }

        // Refresh: context-sensitive. On the left pane, re-sync the selected
        // list (root query); on an item, re-fetch just that item.
        KeyCode::Char('r') if app.focus == Focus::QueryList => {
            return Action::RefreshList;
        }
        KeyCode::Char('r')
            if matches!(app.focus, Focus::ItemList | Focus::ItemDetail)
                && app.selected_item().is_some() =>
        {
            return Action::RefreshItem;
        }
        // Force a full re-fetch + prune of the selected query.
        KeyCode::Char('S') => {
            return Action::FullResync;
        }

        // Apply held-back background updates to the visible list.
        KeyCode::Char('u') if app.pending_count > 0 => {
            return Action::ApplyPending;
        }

        // Toggle desktop notifications and persist the choice.
        KeyCode::Char('N') => {
            app.notifications_enabled = !app.notifications_enabled;
            let mut s = TuiSettings::load();
            s.notifications_enabled = app.notifications_enabled;
            s.save();
            app.status = Some(format!(
                "Desktop notifications {}",
                if app.notifications_enabled {
                    "on"
                } else {
                    "off"
                }
            ));
        }

        // Toggle the icon font and persist the choice.
        KeyCode::Char('F') => {
            let mut s = TuiSettings::load();
            s.use_icon_font = !s.use_icon_font;
            s.save();
            app.icons = Icons::new(s.use_icon_font);
            app.status = Some(format!(
                "Icon font {}",
                if s.use_icon_font { "on" } else { "off" }
            ));
        }

        // Enter filter mode (middle pane)
        KeyCode::Char('/') if app.focus == Focus::ItemList => {
            app.input_mode = InputMode::Filter;
        }

        _ => {}
    }
    Action::None
}

fn handle_key_action_menu(app: &mut App, key: KeyEvent) -> Action {
    let available_len = app
        .selected_item()
        .map(|item| item_actions(&item.kind).len())
        .unwrap_or(0);

    match key.code {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            let max = available_len.saturating_sub(1);
            if app.action_cursor < max {
                app.action_cursor += 1;
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.action_cursor = app.action_cursor.saturating_sub(1);
        }
        KeyCode::Enter => return Action::Confirm,
        _ => {}
    }

    Action::None
}

fn handle_key_custom_action_menu(app: &mut App, key: KeyEvent) -> Action {
    let available_len = app.custom_actions_for_selected().len();

    match key.code {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            let max = available_len.saturating_sub(1);
            if app.custom_action_cursor < max {
                app.custom_action_cursor += 1;
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.custom_action_cursor = app.custom_action_cursor.saturating_sub(1);
        }
        KeyCode::Enter => return Action::ConfirmCustom,
        _ => {}
    }

    Action::None
}

fn handle_key_merge_menu(app: &mut App, key: KeyEvent) -> Action {
    let max = MergeStrategy::all().len().saturating_sub(1);

    match key.code {
        KeyCode::Esc => {
            app.input_mode = InputMode::ActionMenu;
        }
        KeyCode::Char('j') | KeyCode::Down if app.merge_strategy_cursor < max => {
            app.merge_strategy_cursor += 1;
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.merge_strategy_cursor = app.merge_strategy_cursor.saturating_sub(1);
        }
        KeyCode::Enter => return Action::ConfirmMergeStrategy,
        _ => {}
    }

    Action::None
}

fn handle_key_review_menu(app: &mut App, key: KeyEvent) -> Action {
    let max = ReviewEvent::all().len().saturating_sub(1);

    match key.code {
        // The editor already ran, so there is nothing to return to: Esc aborts
        // the whole review.
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
            app.review_body = None;
            app.status = Some("Review cancelled".into());
        }
        KeyCode::Char('j') | KeyCode::Down if app.review_event_cursor < max => {
            app.review_event_cursor += 1;
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.review_event_cursor = app.review_event_cursor.saturating_sub(1);
        }
        KeyCode::Enter => return Action::ConfirmReviewEvent,
        _ => {}
    }

    Action::None
}

fn handle_key_comments_popup(app: &mut App, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.input_mode = InputMode::Normal;
            app.comments.clear();
            app.comments_loading = false;
            app.comments_scroll = 0;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            app.comments_scroll = app.comments_scroll.saturating_add(1);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.comments_scroll = app.comments_scroll.saturating_sub(1);
        }
        KeyCode::Char('g') => {
            app.comments_scroll = 0;
        }
        KeyCode::Char('G') => {
            app.comments_scroll = app.comments_scroll.saturating_add(9999);
        }
        KeyCode::Char('h') => {
            app.comments_show_hidden = !app.comments_show_hidden;
            app.comments_scroll = 0;
        }
        KeyCode::Char('s') => {
            app.comments_sort_desc = !app.comments_sort_desc;
            app.comments_scroll = 0;
        }
        _ => {}
    }
    Action::None
}

fn handle_key_filter(app: &mut App, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
        }
        // Clear the whole filter (matches the "C-u:clear" hint). TextArea's own
        // Ctrl+U is undo, so intercept it here before forwarding.
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.filter = SingleLineInput::new();
            app.item_cursor = 0;
        }
        // Single-line field: never insert a newline or a tab.
        KeyCode::Enter | KeyCode::Tab => {}
        // Everything else (text, Backspace/Delete, cursor moves, Emacs keys) is
        // handled by the TextArea widget's own key bindings. Only reset the item
        // selection when the filter text actually changed — a pure cursor move
        // (Left/Home/Ctrl+A/…) leaves the filtered list unchanged.
        _ => {
            if app.filter.input(key) {
                app.item_cursor = 0;
            }
        }
    }
    Action::None
}

/// Outcome of pressing Enter in a two-field modal.
enum EnterOutcome {
    /// Submit the modal with this action.
    Save(Action),
    /// Move focus to the given field index (0 or 1). Focusing the current field
    /// is the "do nothing" outcome.
    Focus(usize),
}

/// Shared key handling for the two-field input modals (new/edit × query/filter
/// stream). They differ only in the Enter policy and the field pair (resolved
/// via `modal_fields`); `on_enter` receives the two fields' text and the active
/// field index and decides what Enter does. Esc/Tab/Ctrl+U and per-key text
/// forwarding (newline/tab-inserting keys dropped) are common to all four.
fn handle_two_field_modal(
    app: &mut App,
    key: KeyEvent,
    on_enter: impl Fn(&str, &str, usize) -> EnterOutcome,
) -> Action {
    match key.code {
        KeyCode::Esc => {
            // Clear the fields before leaving the modal — `modal_fields` keys off
            // `input_mode`, so it must still be the modal mode here.
            if let Some((f0, f1)) = modal_fields(app) {
                f0.clear();
                f1.clear();
            }
            app.input_mode = InputMode::Normal;
            app.modal_field = 0;
        }
        KeyCode::Tab => {
            app.modal_field = 1 - app.modal_field;
        }
        // Clear the active field, consistent with the filter bar's "C-u:clear"
        // (TextArea's own Ctrl+U is undo).
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            clear_active_modal_field(app);
        }
        KeyCode::Enter => {
            let field = app.modal_field;
            let outcome = match modal_fields(app) {
                Some((f0, f1)) => on_enter(f0.value(), f1.value(), field),
                None => return Action::None,
            };
            match outcome {
                EnterOutcome::Save(action) => return action,
                EnterOutcome::Focus(i) => app.modal_field = i,
            }
        }
        // Text, Backspace/Delete, cursor moves and Emacs keys go to the active
        // field; newline/tab-inserting keys are dropped by `SingleLineInput`.
        _ => {
            let field = app.modal_field;
            if let Some((f0, f1)) = modal_fields(app) {
                let active = if field == 0 { f0 } else { f1 };
                active.input(key);
            }
        }
    }
    Action::None
}

/// Enter policy for the filter-stream modals: save when both fields are
/// non-empty, otherwise focus the empty one.
fn both_required(name: &str, filter: &str, save: Action) -> EnterOutcome {
    let name_present = !name.trim().is_empty();
    let filter_present = !filter.trim().is_empty();
    if name_present && filter_present {
        EnterOutcome::Save(save)
    } else {
        // Focus the first empty field.
        EnterOutcome::Focus(if name_present { 1 } else { 0 })
    }
}

fn handle_key_new_query(app: &mut App, key: KeyEvent) -> Action {
    // field 0 = display name (optional), field 1 = GitHub search query
    handle_two_field_modal(app, key, |_name, query, field| {
        if field == 1 && !query.trim().is_empty() {
            EnterOutcome::Save(Action::SaveNewQuery)
        } else {
            // On the name field, or an empty query, move to (or stay on) the
            // query field.
            EnterOutcome::Focus(1)
        }
    })
}

fn handle_key_edit_query(app: &mut App, key: KeyEvent) -> Action {
    // field 0 = display name, field 1 = GitHub search query
    handle_two_field_modal(app, key, |_name, query, _field| {
        if !query.trim().is_empty() {
            EnterOutcome::Save(Action::SaveEditQuery)
        } else {
            EnterOutcome::Focus(1) // move focus to the query field
        }
    })
}

fn handle_key_new_filter_stream(app: &mut App, key: KeyEvent) -> Action {
    // field 0 = name, field 1 = filter (both required)
    handle_two_field_modal(app, key, |name, filter, _field| {
        both_required(name, filter, Action::SaveNewFilterStream)
    })
}

fn handle_key_edit_filter_stream(app: &mut App, key: KeyEvent) -> Action {
    // field 0 = name, field 1 = filter (both required)
    handle_two_field_modal(app, key, |name, filter, _field| {
        both_required(name, filter, Action::SaveEditFilterStream)
    })
}

// ── Editor / terminal helpers (TUI-only) ─────────────────────────────────────

/// Does NOT suspend/restore the TUI — the caller must do that around this call.
fn run_editor(initial_content: &str) -> anyhow::Result<Option<String>> {
    let cwd = std::env::current_dir()?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let path = cwd.join(format!(".glauca-editor-{}-{nonce}.md", std::process::id()));

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    std::fs::write(&path, initial_content)?;

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    let status = std::process::Command::new(program)
        .args(parts)
        .arg(&path)
        .status()?;

    let result = if status.success() {
        let content = std::fs::read_to_string(&path)?;
        let trimmed = content.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    } else {
        None
    };

    let _ = std::fs::remove_file(&path);
    Ok(result)
}

fn suspend_tui<B: ratatui::backend::Backend + io::Write>(
    terminal: &mut Terminal<B>,
) -> anyhow::Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen
    )?;
    Ok(())
}

fn restore_tui<B: ratatui::backend::Backend + io::Write>(
    terminal: &mut Terminal<B>,
) -> anyhow::Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::EnterAlternateScreen
    )?;
    terminal.clear()?;
    Ok(())
}

/// Actions offered for an item in the TUI action menu. This is the TUI's source
/// of truth (used by the menu render, cursor bounds, and confirm handler): the
/// shared `ItemAction::available_for` plus the TUI-only `ReviewOctorus` for PRs
/// (kept out of `available_for` so it never appears in the GUI menu).
pub(crate) fn item_actions(kind: &str) -> Vec<ItemAction> {
    let mut actions = ItemAction::available_for(kind);
    if kind == "pull_request" {
        actions.push(ItemAction::ReviewOctorus);
    }
    actions
}

/// Launch the external `octorus` (`or`) PR-review TUI for `item`, releasing the
/// terminal while it runs and restoring it afterwards. Returns a status-line
/// message. Requires `or` on PATH (`cargo install octorus`) and an authenticated
/// `gh`.
///
/// Resolves the repo's local checkout via ghq's root rules and passes it as
/// `--working-dir` so octorus operates on the target repo rather than glauca's
/// own CWD. If no local checkout is found, returns without launching.
fn run_octorus_review<B: ratatui::backend::Backend + io::Write>(
    terminal: &mut Terminal<B>,
    item: &ItemEntry,
) -> anyhow::Result<String>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let Some(workdir) = glauca_core::ghq::resolve_local_checkout(&item.repo_owner, &item.repo_name)
    else {
        // ローカルに checkout が無ければ、誤ったディレクトリで octorus を動かさないよう
        // TUI を中断せずにそのまま戻す。
        return Ok(format!(
            "Local checkout for {}/{} not found (searched ghq roots)",
            item.repo_owner, item.repo_name
        ));
    };

    suspend_tui(terminal)?;
    // `--working-dir` は OsStr のまま渡す（非 UTF-8 パスを壊さないため）。
    let result = std::process::Command::new("or")
        .arg("--repo")
        .arg(item.repo_display())
        .arg("--pr")
        .arg(item.number.to_string())
        .arg("--working-dir")
        .arg(&workdir)
        .status();
    restore_tui(terminal)?;

    Ok(match result {
        Ok(status) if status.success() => "Returned from octorus".into(),
        Ok(status) => format!("octorus exited with {status}"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            "octorus (`or`) not found — install with `cargo install octorus`".into()
        }
        Err(e) => format!("Failed to launch octorus: {e}"),
    })
}

// 非同期タスク（run_background_command/execute_*/load_items_task/sync_task/
// sync_worker_task/refresh_timer_task）は glauca_core::engine へ移設（A6）。

// ── Query group reordering helpers ───────────────────────────────────────────

/// Position-swap command for moving the entry at `cursor` up (`down=false`) or
/// down within its group: a query swaps with the adjacent query group, a filter
/// stream with an adjacent sibling under the same parent. `None` if there is no
/// neighbor to swap with. The entries vec is reordered later, when the engine
/// confirms with QueriesSwapped / FilterStreamsSwapped.
fn reorder_command(entries: &[LeftPaneEntry], cursor: usize, down: bool) -> Option<EngineCommand> {
    match entries.get(cursor)? {
        LeftPaneEntry::Query(q) => {
            let current_id = q.id;
            if down {
                let next_query_idx = group_range(entries, cursor).end;
                match entries.get(next_query_idx)? {
                    LeftPaneEntry::Query(nq) => Some(EngineCommand::SwapQueryPositions {
                        upper_id: current_id,
                        lower_id: nq.id,
                        active_id: current_id,
                    }),
                    _ => None,
                }
            } else {
                let prev_idx = entries[..cursor]
                    .iter()
                    .rposition(|e| matches!(e, LeftPaneEntry::Query(_)))?;
                match &entries[prev_idx] {
                    LeftPaneEntry::Query(pq) => Some(EngineCommand::SwapQueryPositions {
                        upper_id: pq.id,
                        lower_id: current_id,
                        active_id: current_id,
                    }),
                    _ => None,
                }
            }
        }
        LeftPaneEntry::FilterStream(fs) => {
            let fs_id = fs.id;
            let parent_id = fs.parent_id;
            if down {
                // Swap with next sibling (same parent, immediately after).
                match entries.get(cursor + 1) {
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
                // Swap with previous sibling (same parent, immediately before).
                match entries.get(cursor - 1) {
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
    }
}

/// What `prepare_selected_entry_load` resolves for the selected left-pane entry:
/// the root query to load items for, plus how to interpret/highlight them.
struct SelectedEntryLoad {
    root_id: i64,
    query_str: Option<String>,
    is_filter_stream: bool,
}

fn prepare_selected_entry_load(app: &mut App) -> Option<SelectedEntryLoad> {
    let entry = app.entries.get(app.entry_cursor)?.clone();
    // Selecting an entry does NOT mark it viewed: the unread badge is kept and
    // cleared per-item as items are read (see `mark_selected_item_read`). Only the
    // stream filter is updated here.
    app.stream_filter = entry.stream_filter().map(|s| s.to_string());
    // Switching entries invalidates any held-back update for the previous one.
    app.clear_pending();

    Some(SelectedEntryLoad {
        root_id: entry.root_query_id(),
        query_str: entry.root_query_str().map(str::to_string),
        is_filter_stream: entry.is_filter_stream(),
    })
}

/// Issue the engine commands to (re)load the currently selected entry: load cached
/// items and—for root queries—sync. With `always_sync`, sync unconditionally (and
/// show the indicator immediately); otherwise sync only if the cache is stale.
/// Returns the root query id when a query (not a filter stream) was selected, so the
/// caller can skip it from the background-refresh sweep.
async fn select_current_entry(app: &mut App, engine: &Engine, always_sync: bool) -> Option<i64> {
    let load = prepare_selected_entry_load(app)?;
    engine
        .send(EngineCommand::LoadCached {
            query_id: load.root_id,
        })
        .await;
    if load.is_filter_stream {
        return None;
    }
    let query_str = load.query_str.clone().unwrap_or_default();
    if always_sync {
        engine
            .send(EngineCommand::Sync {
                query_id: load.root_id,
                query_str,
            })
            .await;
        app.syncing = true;
    } else {
        engine
            .send(EngineCommand::SyncIfStale {
                query_id: load.root_id,
                query_str,
            })
            .await;
    }
    Some(load.root_id)
}

/// The query string of the root query with `root_id`, found in the left-pane
/// entries. Used to re-sync the list backing a filter stream (which has no
/// query string of its own).
fn root_query_str(app: &App, root_id: i64) -> Option<String> {
    app.entries.iter().find_map(|e| match e {
        LeftPaneEntry::Query(q) if q.id == root_id => Some(q.query_str.clone()),
        _ => None,
    })
}

/// Re-sync the list for the currently selected entry (its root query) without
/// resetting the cursor/scroll, so a manual refresh keeps the user's place.
async fn refresh_selected_list(app: &mut App, engine: &Engine) {
    let Some(root_id) = app.entries.get(app.entry_cursor).map(|e| e.root_query_id()) else {
        return;
    };
    let Some(query_str) = root_query_str(app, root_id) else {
        app.status = Some("Nothing to refresh".into());
        return;
    };
    engine
        .send(EngineCommand::Sync {
            query_id: root_id,
            query_str,
        })
        .await;
    app.syncing = true;
}

/// Force a full re-fetch of the selected entry's root query (ignores
/// `last_fetched_at`): re-pages everything and prunes cached items that no longer
/// match the query.
async fn full_resync_selected(app: &mut App, engine: &Engine) {
    let Some(root_id) = app.entries.get(app.entry_cursor).map(|e| e.root_query_id()) else {
        return;
    };
    let Some(query_str) = root_query_str(app, root_id) else {
        app.status = Some("Nothing to resync".into());
        return;
    };
    engine
        .send(EngineCommand::FullResync {
            query_id: root_id,
            query_str,
        })
        .await;
    app.syncing = true;
}

/// Re-fetch just the selected item from GitHub into its query's cache.
async fn refresh_selected_item(app: &mut App, engine: &Engine) {
    let Some(item) = app.selected_item().cloned() else {
        return;
    };
    let Some(query_id) = app.selected_root_query_id() else {
        return;
    };
    engine
        .send(EngineCommand::RefreshItem {
            query_id,
            repo_owner: item.repo_owner.clone(),
            repo_name: item.repo_name.clone(),
            number: item.number,
        })
        .await;
    app.status = Some(format!("Refreshing #{}…", item.number));
}

/// Mark the item under the cursor read (it is shown in the detail pane): record the
/// `updated_at` it was read at, clear its in-memory `is_new`, recompute the current
/// query's unread badges, and persist via the engine (fire-and-forget). No-op if
/// there is no selection or the item is already read (not currently unread).
async fn mark_selected_item_read(app: &mut App, engine: &Engine) {
    let Some(item) = app.selected_item().cloned() else {
        return;
    };
    let Some(idx) = app.items.iter().position(|i| {
        i.repo_owner == item.repo_owner && i.repo_name == item.repo_name && i.number == item.number
    }) else {
        return;
    };
    let row = &mut app.items[idx];
    if !is_item_unread(&row.updated_at, row.last_read_updated_at.as_deref()) {
        return;
    }
    row.last_read_updated_at = Some(row.updated_at.clone());
    row.is_new = false;
    let Some(query_id) = app.selected_root_query_id() else {
        return;
    };
    // Recompute from the live items (compute → insert, to avoid borrowing app
    // mutably while reading app.items/entries).
    let updates = glauca_core::logic::compute_unread_counts(
        &app.entries,
        query_id,
        &app.items,
        app.current_user.as_deref(),
    );
    for (key, unread) in updates {
        app.unread_counts.insert(key, unread);
    }
    engine
        .send(EngineCommand::MarkItemRead {
            query_id,
            repo_owner: item.repo_owner,
            repo_name: item.repo_name,
            number: item.number,
        })
        .await;
}

// ── Main run loop ─────────────────────────────────────────────────────────────

pub async fn run(pool: SqlitePool, gh: Octocrab) -> Result<()> {
    // Set up terminal
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, pool, gh).await;

    // Restore terminal unconditionally
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen
    )?;

    result
}

async fn run_app<B: ratatui::backend::Backend + io::Write>(
    terminal: &mut Terminal<B>,
    pool: SqlitePool,
    gh: Octocrab,
) -> Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    // Start the async engine: builds the left-pane entries, resolves the current
    // user, and spawns the background worker / refresh timer / command loop.
    let tui_settings = TuiSettings::load();
    let (mut engine, init) = Engine::start(pool, gh, tui_settings.sync_interval_secs).await?;

    // Build App from the engine's initial entries (filter streams interleaved).
    let queries: Vec<QueryEntry> = init
        .entries
        .iter()
        .filter_map(|e| match e {
            LeftPaneEntry::Query(q) => Some(q.clone()),
            LeftPaneEntry::FilterStream(_) => None,
        })
        .collect();
    let mut app = App::new(queries);
    app.entries = init.entries;
    app.current_user = init.current_user;
    app.notifications_enabled = tui_settings.notifications_enabled;
    app.icons = Icons::new(tui_settings.use_icon_font);
    app.custom_actions = CustomActions::load();

    // Prime unread counts for every root query via a cached load (no sync).
    let root_query_ids: Vec<i64> = app
        .entries
        .iter()
        .filter_map(|entry| match entry {
            LeftPaneEntry::Query(q) => Some(q.id),
            LeftPaneEntry::FilterStream(_) => None,
        })
        .collect();
    for query_id in &root_query_ids {
        engine
            .send(EngineCommand::LoadCached {
                query_id: *query_id,
            })
            .await;
    }

    // Load items for the initially selected entry; sync only if the cache is stale.
    let initially_synced_id = select_current_entry(&mut app, &engine, false).await;

    // Enqueue all other stale queries for immediate background refresh.
    engine
        .send(EngineCommand::EnqueueStale {
            skip_query_id: initially_synced_id,
        })
        .await;

    let mut events = EventStream::new();

    loop {
        terminal.draw(|f| ui::draw(f, &app))?;

        tokio::select! {
            Some(Ok(event)) = events.next() => {
                if let Event::Key(key) = event {
                    // Ignore key-release events. Terminals with the keyboard-
                    // enhancement protocol (or Windows) emit them, and acting on
                    // both press and release would double-fire actions like
                    // 'd'/'J'/'K'. Repeat events are kept so held-key repeat still
                    // works if enhancement flags are ever enabled.
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    // 'd' in query list → delete selected entry (UI updates on the
                    // QueryDeleted / FilterStreamDeleted message once the DB op succeeds).
                    if key.code == KeyCode::Char('d')
                        && app.focus == Focus::QueryList
                        && app.input_mode == InputMode::Normal
                    {
                        let cmd = match app.entries.get(app.entry_cursor) {
                            Some(LeftPaneEntry::Query(q)) => {
                                Some(EngineCommand::DeleteQuery { query_id: q.id })
                            }
                            Some(LeftPaneEntry::FilterStream(fs)) => {
                                Some(EngineCommand::DeleteFilterStream { id: fs.id })
                            }
                            None => None,
                        };
                        if let Some(cmd) = cmd {
                            engine.send(cmd).await;
                        }
                        continue;
                    }

                    // 'a' in query list → mark all items of the selected entry read.
                    // A query marks its whole root query; a filter stream marks only
                    // its matching items (filter expanded with the current user here,
                    // since the engine does not know `@me`). The engine persists and
                    // reloads the query, which refreshes unread counts via ItemsLoaded.
                    if key.code == KeyCode::Char('a')
                        && app.focus == Focus::QueryList
                        && app.input_mode == InputMode::Normal
                    {
                        let cmd = app.entries.get(app.entry_cursor).map(|entry| {
                            EngineCommand::MarkAllRead {
                                query_id: entry.root_query_id(),
                                filter: entry.stream_filter().map(|f| {
                                    glauca_core::logic::expand_me(app.current_user.as_deref(), f)
                                        .into_owned()
                                }),
                            }
                        });
                        if let Some(cmd) = cmd {
                            engine.send(cmd).await;
                        }
                        continue;
                    }

                    // J/K: move selected entry up/down within its group. The DB swap
                    // runs through the engine; the entries vec is reordered on the
                    // QueriesSwapped / FilterStreamsSwapped confirmation message.
                    if (key.code == KeyCode::Char('J') || key.code == KeyCode::Char('K'))
                        && app.focus == Focus::QueryList
                        && app.input_mode == InputMode::Normal
                    {
                        let down = key.code == KeyCode::Char('J');
                        if let Some(cmd) = reorder_command(&app.entries, app.entry_cursor, down) {
                            engine.send(cmd).await;
                        }
                        continue;
                    }

                    let action = handle_key(&mut app, key);
                    // Keep the visible text cursor on the active modal field.
                    sync_modal_cursors(&mut app);
                    match action {
                        Action::Quit => break,
                        Action::LoadEntry => {
                            app.filter = SingleLineInput::new();
                            app.item_cursor = 0;
                            app.detail_scroll = 0;
                            app.clear_items();
                            select_current_entry(&mut app, &engine, true).await;
                        }
                        Action::SaveNewQuery => {
                            let query_str = app.new_query_input.value().trim().to_string();
                            let name_str = app.new_query_name.value().trim().to_string();
                            app.input_mode = InputMode::Normal;
                            app.modal_field = 0;
                            app.new_query_input = SingleLineInput::new();
                            app.new_query_name = SingleLineInput::new();
                            let name = if name_str.is_empty() {
                                None
                            } else {
                                Some(name_str)
                            };
                            engine
                                .send(EngineCommand::AddQuery {
                                    name,
                                    query: query_str,
                                })
                                .await;
                        }
                        Action::SaveNewFilterStream => {
                            let name = app.new_filter_stream_name.value().trim().to_string();
                            let filter =
                                app.new_filter_stream_filter.value().trim().to_string();
                            app.input_mode = InputMode::Normal;
                            app.new_filter_stream_name = SingleLineInput::new();
                            app.new_filter_stream_filter = SingleLineInput::new();

                            // Determine parent: root_query_id of the currently selected entry
                            if let Some(entry) = app.entries.get(app.entry_cursor) {
                                let parent_id = entry.root_query_id();
                                let kind = entry.kind().to_string();
                                engine
                                    .send(EngineCommand::AddFilterStream {
                                        parent_id,
                                        kind,
                                        name,
                                        filter,
                                    })
                                    .await;
                            }
                        }
                        Action::SaveEditFilterStream => {
                            let name = app.edit_input.value().trim().to_string();
                            let filter = app.edit_input2.value().trim().to_string();
                            app.input_mode = InputMode::Normal;
                            app.edit_input = SingleLineInput::new();
                            app.edit_input2 = SingleLineInput::new();

                            if let Some(LeftPaneEntry::FilterStream(fs)) =
                                app.entries.get(app.entry_cursor)
                            {
                                engine
                                    .send(EngineCommand::EditFilterStream {
                                        id: fs.id,
                                        name,
                                        filter,
                                    })
                                    .await;
                            }
                        }
                        Action::SaveEditQuery => {
                            let name_input = app.edit_input.value().trim().to_string();
                            let new_query = app.edit_input2.value().trim().to_string();
                            app.input_mode = InputMode::Normal;
                            app.edit_input = SingleLineInput::new();
                            app.edit_input2 = SingleLineInput::new();

                            if let Some(LeftPaneEntry::Query(q)) =
                                app.entries.get(app.entry_cursor)
                            {
                                // Empty name means "use query string as label"
                                let new_name: Option<String> = if name_input.is_empty() {
                                    None
                                } else {
                                    Some(name_input)
                                };
                                engine
                                    .send(EngineCommand::EditQuery {
                                        id: q.id,
                                        name: new_name,
                                        query: new_query,
                                    })
                                    .await;
                            }
                        }
                        Action::Confirm => {
                            if let Some(item) = app.selected_item().cloned() {
                                let actions = item_actions(&item.kind);
                                if let Some(action) = actions.get(app.action_cursor).cloned() {
                                    match action {
                                        ItemAction::OpenBrowser => {
                                            app.input_mode = InputMode::Normal;
                                            engine
                                                .send(EngineCommand::OpenBrowser {
                                                    item: Box::new(item.clone()),
                                                })
                                                .await;
                                        }
                                        ItemAction::Comment => {
                                            app.input_mode = InputMode::Normal;
                                            suspend_tui(terminal)?;
                                            let editor_result = run_editor("");
                                            restore_tui(terminal)?;

                                            match editor_result {
                                                Ok(Some(body)) => {
                                                    engine
                                                        .send(EngineCommand::Comment {
                                                            url: item.url.clone(),
                                                            kind: item.kind.clone(),
                                                            body,
                                                        })
                                                        .await;
                                                }
                                                Ok(None) => {
                                                    app.status = Some("Comment cancelled".into());
                                                }
                                                Err(e) => {
                                                    app.status = Some(format!("Editor error: {e}"));
                                                }
                                            }
                                        }
                                        ItemAction::ViewComments => {
                                            // Open in-TUI comments popup: fetch via API in background
                                            app.input_mode = InputMode::CommentsPopup;
                                            app.comments.clear();
                                            app.comments_loading = true;
                                            app.comments_scroll = 0;
                                            engine
                                                .send(EngineCommand::LoadComments {
                                                    owner: item.repo_owner.clone(),
                                                    repo: item.repo_name.clone(),
                                                    number: item.number as u64,
                                                })
                                                .await;
                                        }
                                        ItemAction::ApprovePR => {
                                            app.input_mode = InputMode::Normal;
                                            suspend_tui(terminal)?;
                                            let editor_result = run_editor(
                                                "# Review comment (required for Comment / Request changes; optional for Approve)\n# Lines starting with '#' are ignored.\n",
                                            );
                                            restore_tui(terminal)?;

                                            match editor_result {
                                                Ok(body_opt) => {
                                                    // Strip comment lines; empty → no body. Then
                                                    // confirm the review event before submitting.
                                                    app.review_body = body_opt.and_then(|body| {
                                                        let stripped = body
                                                            .lines()
                                                            .filter(|line| !line.starts_with('#'))
                                                            .collect::<Vec<_>>()
                                                            .join("\n");
                                                        let stripped = stripped.trim().to_string();
                                                        (!stripped.is_empty()).then_some(stripped)
                                                    });
                                                    app.review_event_cursor = 0;
                                                    app.input_mode = InputMode::ReviewMenu;
                                                }
                                                Err(e) => {
                                                    app.status = Some(format!("Editor error: {e}"));
                                                }
                                            }
                                        }
                                        ItemAction::MergePR => {
                                            app.input_mode = InputMode::MergeMenu;
                                            app.merge_strategy_cursor = 0;
                                        }
                                        ItemAction::CopyUrl => {
                                            app.input_mode = InputMode::Normal;
                                            app.status = Some(match copy_to_clipboard_osc52(&item.url) {
                                                Ok(()) => "Copied URL to clipboard".into(),
                                                Err(e) => format!("Copy failed: {e}"),
                                            });
                                        }
                                        ItemAction::ReviewOctorus => {
                                            app.input_mode = InputMode::Normal;
                                            app.status = Some(run_octorus_review(terminal, &item)?);
                                        }
                                        ItemAction::RefreshItem => {
                                            app.input_mode = InputMode::Normal;
                                            refresh_selected_item(&mut app, &engine).await;
                                        }
                                    }
                                }
                            }
                        }
                        Action::ConfirmMergeStrategy => {
                            if let Some(item) = app.selected_item().cloned() {
                                let strategies = MergeStrategy::all();
                                if let Some(strategy) = strategies.get(app.merge_strategy_cursor).cloned() {
                                    app.input_mode = InputMode::Normal;
                                    engine
                                        .send(EngineCommand::Merge {
                                            url: item.url.clone(),
                                            strategy,
                                        })
                                        .await;
                                }
                            }
                        }
                        Action::ConfirmReviewEvent => {
                            if let Some(item) = app.selected_item().cloned()
                                && let Some(event) =
                                    ReviewEvent::all().get(app.review_event_cursor).copied()
                            {
                                // gh requires a body for comment / request-changes.
                                if event.requires_body() && app.review_body.is_none() {
                                    app.status = Some(
                                        "Review comment required for Comment / Request changes"
                                            .into(),
                                    );
                                } else {
                                    app.input_mode = InputMode::Normal;
                                    let body = app.review_body.take();
                                    engine
                                        .send(EngineCommand::SubmitReview {
                                            url: item.url.clone(),
                                            event,
                                            body,
                                        })
                                        .await;
                                }
                            }
                        }
                        Action::OpenBrowser => {
                            if let Some(item) = app.selected_item().cloned() {
                                engine
                                    .send(EngineCommand::OpenBrowser {
                                        item: Box::new(item),
                                    })
                                    .await;
                            }
                        }
                        Action::CopyUrl => {
                            if let Some(item) = app.selected_item().cloned() {
                                app.status = Some(match copy_to_clipboard_osc52(&item.url) {
                                    Ok(()) => "Copied URL to clipboard".into(),
                                    Err(e) => format!("Copy failed: {e}"),
                                });
                            }
                        }
                        Action::ConfirmCustom => {
                            let action = app
                                .custom_actions_for_selected()
                                .get(app.custom_action_cursor)
                                .map(|&a| a.clone());
                            if let (Some(action), Some(item)) =
                                (action, app.selected_item().cloned())
                            {
                                app.input_mode = InputMode::Normal;
                                engine
                                    .send(EngineCommand::RunCustomAction {
                                        action: Box::new(action),
                                        item: Box::new(item),
                                    })
                                    .await;
                            }
                        }
                        Action::ReviewOctorus => {
                            if let Some(item) = app.selected_item().cloned() {
                                app.status = Some(run_octorus_review(terminal, &item)?);
                            }
                        }
                        Action::RefreshList => {
                            refresh_selected_list(&mut app, &engine).await;
                        }
                        Action::RefreshItem => {
                            refresh_selected_item(&mut app, &engine).await;
                        }
                        Action::FullResync => {
                            full_resync_selected(&mut app, &engine).await;
                        }
                        Action::ApplyPending => {
                            app.apply_pending_items();
                        }
                        Action::None => {}
                    }
                    // Viewing an item (cursor on the item list or its detail pane)
                    // marks it read and decrements the unread badge. Idempotent —
                    // a no-op once the item is already read.
                    if app.input_mode == InputMode::Normal
                        && matches!(app.focus, Focus::ItemList | Focus::ItemDetail)
                    {
                        mark_selected_item_read(&mut app, &engine).await;
                    }
                }
            }
            Some(msg) = engine.recv() => {
                match msg {
                    AppMessage::ItemsLoaded {
                        query_id,
                        items,
                        background,
                    } => {
                        // Desktop notification, independent of which query is
                        // selected. Returns `None` on the query's first load this
                        // session (baseline only), suppressing the startup storm.
                        let to_notify = app
                            .notif_tracker
                            .changed_count_to_notify(
                                query_id,
                                &items,
                                background,
                                app.notifications_enabled,
                            )
                            .and_then(|n| {
                                query_label(&app.entries, query_id).map(|name| (name, n))
                            });
                        if let Some((name, n)) = to_notify {
                            tokio::task::spawn_blocking(move || {
                                glauca_core::notify::notify_updated_items(&name, n)
                            });
                        }
                        let is_current = app.selected_root_query_id() == Some(query_id);
                        if is_current && background {
                            // Don't change the list under the user; stash the fresh
                            // items and show a "N updated" banner (applied via `u`).
                            let n = glauca_core::logic::count_changed(&app.items, &items);
                            if n == 0 {
                                app.clear_pending();
                            } else {
                                app.pending_items = Some(items);
                                app.pending_count = n;
                            }
                        } else {
                            app.recompute_unread_counts_for_query(query_id, &items);
                            if is_current {
                                // Foreground load: apply live and drop any banner.
                                app.apply_items_to_view(items);
                                app.clear_pending();
                            }
                        }
                    }
                    AppMessage::QueryAdded(q) => {
                        app.entries.push(LeftPaneEntry::Query(q));
                        app.entry_cursor = app.entries.len() - 1;
                        app.clear_items();
                        app.filter = SingleLineInput::new();
                        app.stream_filter = None;
                        select_current_entry(&mut app, &engine, true).await;
                    }
                    AppMessage::FilterStreamAdded(fs) => {
                        // Insert the filter stream after the last sibling (or after its parent)
                        let insert_pos = app
                            .entries
                            .iter()
                            .rposition(|e| e.root_query_id() == fs.parent_id)
                            .map(|p| p + 1)
                            .unwrap_or(app.entries.len());
                        app.entries.insert(insert_pos, LeftPaneEntry::FilterStream(fs));
                        // Select the newly added filter stream
                        app.entry_cursor = insert_pos;
                        app.filter = SingleLineInput::new();
                        app.item_cursor = 0;
                        app.detail_scroll = 0;
                        select_current_entry(&mut app, &engine, true).await;
                    }
                    AppMessage::QueryUpdated { id, new_name, new_query } => {
                        if let Some(LeftPaneEntry::Query(q)) = app
                            .entries
                            .iter_mut()
                            .find(|e| matches!(e, LeftPaneEntry::Query(q) if q.id == id))
                        {
                            q.label = new_name.clone().unwrap_or_else(|| new_query.clone());
                            q.query_str = new_query.clone();
                        }
                        // Reload + sync with the new query string
                        if app.selected_root_query_id() == Some(id) {
                            app.clear_items();
                            app.item_cursor = 0;
                            app.detail_scroll = 0;
                            app.filter = SingleLineInput::new();
                            engine
                                .send(EngineCommand::LoadCached { query_id: id })
                                .await;
                            engine
                                .send(EngineCommand::Sync {
                                    query_id: id,
                                    query_str: new_query,
                                })
                                .await;
                            app.syncing = true;
                        }
                        app.status = Some("Query updated".into());
                    }
                    AppMessage::FilterStreamUpdated { id, new_name, new_filter } => {
                        if let Some(LeftPaneEntry::FilterStream(fs)) = app
                            .entries
                            .iter_mut()
                            .find(|e| matches!(e, LeftPaneEntry::FilterStream(fs) if fs.id == id))
                        {
                            fs.name = new_name;
                            fs.filter = new_filter.clone();
                        }
                        // If this filter stream is currently selected, re-apply its filter
                        if let Some(LeftPaneEntry::FilterStream(fs)) =
                            app.entries.get(app.entry_cursor)
                            && fs.id == id
                        {
                            app.stream_filter = Some(new_filter.clone());
                            app.item_cursor = 0;
                            app.detail_scroll = 0;
                            app.clamp_item_cursor();
                        }
                        if let Some(root_id) = app
                            .entries
                            .iter()
                            .find_map(|entry| match entry {
                                LeftPaneEntry::FilterStream(fs) if fs.id == id => Some(fs.parent_id),
                                _ => None,
                            })
                        {
                            let items = app.items.clone();
                            app.recompute_unread_counts_for_query(root_id, &items);
                        }
                        app.status = Some("Filter stream updated".into());
                    }
                    AppMessage::Status(s) => {
                        app.status = Some(s);
                    }
                    AppMessage::ActionDone(msg) => {
                        app.status = Some(msg);
                    }
                    AppMessage::ActionError(err) => {
                        app.status = Some(format!("Error: {err}"));
                    }
                    AppMessage::CommentsLoaded(comments) => {
                        app.comments = comments;
                        app.comments_loading = false;
                    }
                    AppMessage::CommentsFailed(err) => {
                        app.comments_loading = false;
                        // Stay in CommentsPopup so the user sees the error; show it as a comment
                        app.comments = vec![CommentEntry {
                            author: "error".into(),
                            created_at: String::new(),
                            body: format!("Failed to load comments: {err}"),
                            is_minimized: false,
                            minimized_reason: None,
                        }];
                    }
                    AppMessage::SyncDone { query_id, count } => {
                        if app.selected_root_query_id() == Some(query_id) {
                            app.syncing = false;
                            app.status = Some(format!("Synced {count} items"));
                        }
                    }
                    AppMessage::SyncError { query_id, error, .. } => {
                        if app.selected_root_query_id() == Some(query_id) {
                            app.syncing = false;
                        }
                        app.status = Some(format!("Sync error: {error}"));
                    }
                    AppMessage::BgSyncQueued(n) => {
                        app.bg_sync_pending += n;
                    }
                    AppMessage::BgSyncJobDone => {
                        app.bg_sync_pending = app.bg_sync_pending.saturating_sub(1);
                    }
                    AppMessage::SyncStarted { query_id } => {
                        if app.selected_root_query_id() == Some(query_id) {
                            app.syncing = true;
                        }
                    }
                    AppMessage::QueryDeleted { query_id } => {
                        // Remove all entries for this root query and its streams.
                        app.entries.retain(|e| e.root_query_id() != query_id);
                        app.entry_cursor = app
                            .entry_cursor
                            .min(app.entries.len().saturating_sub(1));
                        app.clear_items();
                        app.item_cursor = 0;
                        app.filter = SingleLineInput::new();
                        app.stream_filter = None;
                        select_current_entry(&mut app, &engine, true).await;
                    }
                    AppMessage::FilterStreamDeleted { id } => {
                        app.entries.retain(|e| e.id() != id);
                        app.entry_cursor = app
                            .entry_cursor
                            .min(app.entries.len().saturating_sub(1));
                        app.clear_items();
                        app.item_cursor = 0;
                        app.filter = SingleLineInput::new();
                        app.stream_filter = None;
                        select_current_entry(&mut app, &engine, true).await;
                    }
                    AppMessage::QueriesSwapped { upper_id, active_id, .. } => {
                        // Move the upper group down past the next group, then follow the
                        // active query with the cursor.
                        if let Some(idx) = app.entries.iter().position(
                            |e| matches!(e, LeftPaneEntry::Query(q) if q.id == upper_id),
                        ) {
                            move_group_down(&mut app.entries, idx);
                            if let Some(new_cursor) = app.entries.iter().position(
                                |e| matches!(e, LeftPaneEntry::Query(q) if q.id == active_id),
                            ) {
                                app.entry_cursor = new_cursor;
                            }
                        }
                    }
                    AppMessage::FilterStreamsSwapped { upper_id, lower_id, active_id } => {
                        let upper_idx = app.entries.iter().position(
                            |e| matches!(e, LeftPaneEntry::FilterStream(fs) if fs.id == upper_id),
                        );
                        let lower_idx = app.entries.iter().position(
                            |e| matches!(e, LeftPaneEntry::FilterStream(fs) if fs.id == lower_id),
                        );
                        if let (Some(u), Some(l)) = (upper_idx, lower_idx) {
                            app.entries.swap(u, l);
                            if let Some(new_cursor) =
                                app.entries.iter().position(|e| e.id() == active_id)
                            {
                                app.entry_cursor = new_cursor;
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::*;

    // ── handle_key_new_query ─────────────────────────────────────────────────────

    #[test]
    fn question_mark_opens_help_overlay() {
        let mut app = App::new(vec![]);
        handle_key_normal(&mut app, make_key(KeyCode::Char('?')));
        assert!(matches!(app.input_mode, InputMode::Help));
    }

    #[test]
    fn help_overlay_closes_on_esc_question_and_q() {
        for close_key in [KeyCode::Esc, KeyCode::Char('?'), KeyCode::Char('q')] {
            let mut app = App::new(vec![]);
            app.input_mode = InputMode::Help;
            handle_key_help(&mut app, make_key(close_key));
            assert!(
                matches!(app.input_mode, InputMode::Normal),
                "{close_key:?} should close help"
            );
        }
    }

    #[test]
    fn help_overlay_ignores_other_keys() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::Help;
        handle_key_help(&mut app, make_key(KeyCode::Char('j')));
        assert!(matches!(app.input_mode, InputMode::Help));
    }

    #[test]
    fn new_query_enter_on_field0_moves_to_field1() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::NewQuery;
        app.modal_field = 0;
        let action = handle_key_new_query(&mut app, make_key(KeyCode::Enter));
        assert!(matches!(action, Action::None));
        assert_eq!(app.modal_field, 1);
        assert!(matches!(app.input_mode, InputMode::NewQuery));
    }

    #[test]
    fn new_query_enter_on_field1_empty_query_no_save() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::NewQuery;
        app.modal_field = 1;
        app.new_query_input = SingleLineInput::new();
        let action = handle_key_new_query(&mut app, make_key(KeyCode::Enter));
        assert!(matches!(action, Action::None));
    }

    #[test]
    fn new_query_enter_on_field1_with_query_saves() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::NewQuery;
        app.modal_field = 1;
        app.new_query_input = ta("is:pr is:open");
        let action = handle_key_new_query(&mut app, make_key(KeyCode::Enter));
        assert!(matches!(action, Action::SaveNewQuery));
    }

    #[test]
    fn new_query_esc_clears_and_exits() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::NewQuery;
        app.modal_field = 1;
        app.new_query_name = ta("My name");
        app.new_query_input = ta("is:pr");
        handle_key_new_query(&mut app, make_key(KeyCode::Esc));
        assert!(matches!(app.input_mode, InputMode::Normal));
        assert!(app.new_query_name.is_empty());
        assert!(app.new_query_input.is_empty());
        assert_eq!(app.modal_field, 0);
    }

    #[test]
    fn new_query_tab_toggles_field() {
        let mut app = App::new(vec![]);
        app.modal_field = 0;
        handle_key_new_query(&mut app, make_key(KeyCode::Tab));
        assert_eq!(app.modal_field, 1);
        handle_key_new_query(&mut app, make_key(KeyCode::Tab));
        assert_eq!(app.modal_field, 0);
    }

    // ── handle_key_filter ────────────────────────────────────────────────────────

    #[test]
    fn filter_esc_exits_mode() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::Filter;
        handle_key_filter(&mut app, make_key(KeyCode::Esc));
        assert!(matches!(app.input_mode, InputMode::Normal));
    }

    #[test]
    fn filter_backspace_removes_last_char() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::Filter;
        app.filter = ta("fix");
        handle_key_filter(&mut app, make_key(KeyCode::Backspace));
        assert_eq!(app.filter.value(), "fi");
    }

    #[test]
    fn filter_ctrl_u_clears_filter() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::Filter;
        app.filter = ta("some filter text");
        handle_key_filter(&mut app, make_ctrl_key(KeyCode::Char('u')));
        assert!(app.filter.is_empty());
    }

    #[test]
    fn filter_char_appends() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::Filter;
        app.filter = ta("fi");
        handle_key_filter(&mut app, make_key(KeyCode::Char('x')));
        assert_eq!(app.filter.value(), "fix");
    }

    // ── cursor movement (wiring to the TextArea widget) ──────────────────────────

    #[test]
    fn filter_left_then_insert_goes_midway() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::Filter;
        app.filter = ta("fix"); // cursor at end
        handle_key_filter(&mut app, make_key(KeyCode::Left));
        handle_key_filter(&mut app, make_key(KeyCode::Char('e')));
        assert_eq!(app.filter.value(), "fiex");
    }

    #[test]
    fn filter_home_then_insert_prepends() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::Filter;
        app.filter = ta("fix");
        handle_key_filter(&mut app, make_key(KeyCode::Home));
        handle_key_filter(&mut app, make_key(KeyCode::Char('z')));
        assert_eq!(app.filter.value(), "zfix");
    }

    #[test]
    fn filter_ctrl_a_moves_to_line_start() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::Filter;
        app.filter = ta("fix");
        handle_key_filter(&mut app, make_ctrl_key(KeyCode::Char('a')));
        handle_key_filter(&mut app, make_key(KeyCode::Char('z')));
        assert_eq!(app.filter.value(), "zfix");
    }

    #[test]
    fn filter_delete_removes_char_at_cursor() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::Filter;
        app.filter = ta("fix");
        handle_key_filter(&mut app, make_key(KeyCode::Home));
        handle_key_filter(&mut app, make_key(KeyCode::Delete));
        assert_eq!(app.filter.value(), "ix");
    }

    #[test]
    fn filter_enter_does_not_insert_newline() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::Filter;
        app.filter = ta("fix");
        handle_key_filter(&mut app, make_key(KeyCode::Enter));
        assert_eq!(app.filter.value(), "fix");
    }

    #[test]
    fn filter_move_left_respects_multibyte_boundary() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::Filter;
        app.filter = ta("あい"); // cursor at end
        handle_key_filter(&mut app, make_key(KeyCode::Left));
        handle_key_filter(&mut app, make_key(KeyCode::Char('う')));
        assert_eq!(app.filter.value(), "あうい");
    }

    #[test]
    fn new_query_forwards_edit_to_active_field() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::NewQuery;
        app.modal_field = 1;
        app.new_query_input = ta("ab"); // cursor at end
        handle_key_new_query(&mut app, make_key(KeyCode::Left));
        handle_key_new_query(&mut app, make_key(KeyCode::Char('X')));
        assert_eq!(app.new_query_input.value(), "aXb");
        // The inactive name field is untouched.
        assert!(app.new_query_name.is_empty());
    }

    // ── single-line invariant: newline/tab keys must never split the field ───────

    #[test]
    fn filter_ctrl_m_does_not_insert_newline() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::Filter;
        app.filter = ta("fix");
        handle_key_filter(&mut app, make_ctrl_key(KeyCode::Char('m')));
        assert_eq!(app.filter.value(), "fix");
    }

    #[test]
    fn filter_literal_newline_char_does_not_split() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::Filter;
        app.filter = ta("fix");
        handle_key_filter(&mut app, make_key(KeyCode::Char('\n')));
        handle_key_filter(&mut app, make_key(KeyCode::Char('\r')));
        assert_eq!(app.filter.value(), "fix");
    }

    // ── Enter policies for the other three modals ───────────────────────────────

    #[test]
    fn edit_query_enter_saves_when_query_nonempty() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::EditQuery;
        app.modal_field = 0; // even while on the name field
        app.edit_input = ta("My name");
        app.edit_input2 = ta("is:pr is:open");
        let action = handle_key_edit_query(&mut app, make_key(KeyCode::Enter));
        assert!(matches!(action, Action::SaveEditQuery));
    }

    #[test]
    fn edit_query_enter_empty_query_focuses_query_field() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::EditQuery;
        app.modal_field = 0;
        app.edit_input = ta("My name");
        app.edit_input2 = SingleLineInput::new(); // empty query
        let action = handle_key_edit_query(&mut app, make_key(KeyCode::Enter));
        assert!(matches!(action, Action::None));
        assert_eq!(app.modal_field, 1);
    }

    #[test]
    fn new_filter_stream_enter_saves_when_both_filled() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::NewFilterStream;
        app.new_filter_stream_name = ta("Drafts");
        app.new_filter_stream_filter = ta("is:draft");
        let action = handle_key_new_filter_stream(&mut app, make_key(KeyCode::Enter));
        assert!(matches!(action, Action::SaveNewFilterStream));
    }

    #[test]
    fn new_filter_stream_enter_focuses_empty_field() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::NewFilterStream;
        app.modal_field = 1;
        app.new_filter_stream_name = SingleLineInput::new(); // name empty
        app.new_filter_stream_filter = ta("is:draft");
        let action = handle_key_new_filter_stream(&mut app, make_key(KeyCode::Enter));
        assert!(matches!(action, Action::None));
        assert_eq!(app.modal_field, 0); // jumps to the empty name field
    }

    #[test]
    fn edit_filter_stream_enter_saves_when_both_filled() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::EditFilterStream;
        app.edit_input = ta("Drafts");
        app.edit_input2 = ta("is:draft");
        let action = handle_key_edit_filter_stream(&mut app, make_key(KeyCode::Enter));
        assert!(matches!(action, Action::SaveEditFilterStream));
    }

    // ── modal field routing (non-NewQuery modes) and cursor sync ─────────────────

    #[test]
    fn edit_query_ctrl_u_clears_field0_only() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::EditQuery;
        app.modal_field = 0;
        app.edit_input = ta("clear me");
        app.edit_input2 = ta("is:pr");
        handle_key_edit_query(&mut app, make_ctrl_key(KeyCode::Char('u')));
        assert!(app.edit_input.is_empty());
        assert_eq!(app.edit_input2.value(), "is:pr");
    }

    #[test]
    fn edit_filter_stream_forwards_edit_to_active_field() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::EditFilterStream;
        app.modal_field = 1;
        app.edit_input = ta("name");
        app.edit_input2 = ta("is:pr");
        handle_key_edit_filter_stream(&mut app, make_key(KeyCode::Char('x')));
        assert_eq!(app.edit_input2.value(), "is:prx");
        assert_eq!(app.edit_input.value(), "name"); // other field untouched
    }

    #[test]
    fn new_query_ctrl_u_clears_active_field() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::NewQuery;
        app.modal_field = 1;
        app.new_query_name = ta("keep me");
        app.new_query_input = ta("is:pr is:open");
        handle_key_new_query(&mut app, make_ctrl_key(KeyCode::Char('u')));
        // Active field (1) cleared; the other field is untouched.
        assert!(app.new_query_input.is_empty());
        assert_eq!(app.new_query_name.value(), "keep me");
    }

    #[test]
    fn new_query_back_tab_does_not_insert_tab() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::NewQuery;
        app.modal_field = 0;
        handle_key_new_query(&mut app, make_key(KeyCode::BackTab));
        assert_eq!(app.new_query_name.value(), "");
    }

    // ── item selection is preserved on pure cursor moves in the filter ───────────

    #[test]
    fn filter_cursor_move_keeps_item_cursor() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::Filter;
        app.filter = ta("fix");
        app.item_cursor = 3;
        // Pure cursor moves must not reset the item selection.
        handle_key_filter(&mut app, make_key(KeyCode::Left));
        handle_key_filter(&mut app, make_key(KeyCode::Home));
        assert_eq!(app.item_cursor, 3);
        // An actual text edit does reset it.
        handle_key_filter(&mut app, make_key(KeyCode::Char('z')));
        assert_eq!(app.item_cursor, 0);
    }

    #[test]
    fn item_action_available_for_kind_is_context_aware() {
        assert_eq!(
            ItemAction::available_for("pull_request"),
            vec![
                ItemAction::OpenBrowser,
                ItemAction::CopyUrl,
                ItemAction::RefreshItem,
                ItemAction::ViewComments,
                ItemAction::Comment,
                ItemAction::ApprovePR,
                ItemAction::MergePR,
            ]
        );
        assert_eq!(
            ItemAction::available_for("issue"),
            vec![
                ItemAction::OpenBrowser,
                ItemAction::CopyUrl,
                ItemAction::RefreshItem,
                ItemAction::ViewComments,
                ItemAction::Comment,
            ]
        );
    }

    #[test]
    fn refresh_item_available_for_both_kinds() {
        assert!(ItemAction::available_for("pull_request").contains(&ItemAction::RefreshItem));
        assert!(ItemAction::available_for("issue").contains(&ItemAction::RefreshItem));
    }

    #[test]
    fn item_actions_appends_octorus_for_prs_only() {
        let pr = item_actions("pull_request");
        assert_eq!(pr.last(), Some(&ItemAction::ReviewOctorus));
        // Only PRs get it.
        assert!(!item_actions("issue").contains(&ItemAction::ReviewOctorus));
        // It is a TUI-only addition, never surfaced by the shared `available_for`.
        assert!(!ItemAction::available_for("pull_request").contains(&ItemAction::ReviewOctorus));
    }

    #[test]
    fn osc52_sequence_wraps_base64() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let url = "https://github.com/owner/repo/pull/1";
        let seq = osc52_sequence(url);
        let expected = format!("\x1b]52;c;{}\x07", STANDARD.encode(url.as_bytes()));
        assert_eq!(seq, expected);
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with('\x07'));
    }

    #[test]
    fn enter_opens_action_menu_for_selected_item() {
        let mut app = make_app_with_items(&["First"]);
        app.focus = Focus::ItemList;

        let action = handle_key_normal(&mut app, make_key(KeyCode::Enter));

        assert!(matches!(action, Action::None));
        assert_eq!(app.input_mode, InputMode::ActionMenu);
        assert_eq!(app.action_cursor, 0);
    }

    #[test]
    fn action_menu_navigation_and_confirm_work() {
        let mut app = make_app_with_items(&["First"]);
        app.input_mode = InputMode::ActionMenu;

        handle_key_action_menu(&mut app, make_key(KeyCode::Down));
        assert_eq!(app.action_cursor, 1);

        let action = handle_key_action_menu(&mut app, make_key(KeyCode::Enter));
        assert!(matches!(action, Action::Confirm));
    }

    #[test]
    fn x_opens_custom_action_menu_when_an_action_matches() {
        let mut app = make_app_with_items(&["First"]);
        app.focus = Focus::ItemList;
        app.custom_actions = CustomActions {
            actions: vec![make_custom_action("review", &["pull_request"])],
        };

        let action = handle_key_normal(&mut app, make_key(KeyCode::Char('x')));

        assert!(matches!(action, Action::None));
        assert_eq!(app.input_mode, InputMode::CustomActionMenu);
        assert_eq!(app.custom_action_cursor, 0);
    }

    #[test]
    fn x_stays_normal_with_status_when_no_action_matches() {
        let mut app = make_app_with_items(&["First"]); // items are PRs
        app.focus = Focus::ItemList;
        app.custom_actions = CustomActions {
            actions: vec![make_custom_action("issue-only", &["issue"])],
        };

        let action = handle_key_normal(&mut app, make_key(KeyCode::Char('x')));

        assert!(matches!(action, Action::None));
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(app.status.is_some());
    }

    #[test]
    fn custom_action_menu_navigation_and_confirm_work() {
        let mut app = make_app_with_items(&["First"]);
        app.custom_actions = CustomActions {
            actions: vec![make_custom_action("a", &[]), make_custom_action("b", &[])],
        };
        app.input_mode = InputMode::CustomActionMenu;

        handle_key_custom_action_menu(&mut app, make_key(KeyCode::Down));
        assert_eq!(app.custom_action_cursor, 1);
        // Cursor clamps at the last entry.
        handle_key_custom_action_menu(&mut app, make_key(KeyCode::Down));
        assert_eq!(app.custom_action_cursor, 1);

        let action = handle_key_custom_action_menu(&mut app, make_key(KeyCode::Enter));
        assert!(matches!(action, Action::ConfirmCustom));
    }

    #[test]
    fn custom_action_menu_escape_returns_to_normal() {
        let mut app = make_app_with_items(&["First"]);
        app.input_mode = InputMode::CustomActionMenu;

        let action = handle_key_custom_action_menu(&mut app, make_key(KeyCode::Esc));

        assert!(matches!(action, Action::None));
        assert_eq!(app.input_mode, InputMode::Normal);
    }

    #[test]
    fn merge_menu_escape_returns_to_action_menu() {
        let mut app = make_app_with_items(&["First"]);
        app.input_mode = InputMode::MergeMenu;
        app.merge_strategy_cursor = 1;

        let action = handle_key_merge_menu(&mut app, make_key(KeyCode::Esc));

        assert!(matches!(action, Action::None));
        assert_eq!(app.input_mode, InputMode::ActionMenu);
        assert_eq!(app.merge_strategy_cursor, 1);
    }
}
