//! Key event handling: the per-`InputMode` dispatch and the individual mode
//! handlers (normal, filter, the menus, and the shared two-field modal logic).
//! Handlers mutate `App` and return an `Action` for the run loop to carry out.

use super::*;

// ── Key event handler ────────────────────────────────────────────────────────

pub(crate) fn handle_key(app: &mut App, key: KeyEvent) -> Action {
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
        // Esc, Tab, or Enter leaves the filter field. The filter text is kept,
        // so the item list stays filtered and focus remains on the item list.
        // (This also means Enter never inserts a newline into the single-line
        // field.)
        KeyCode::Esc | KeyCode::Tab | KeyCode::Enter => {
            app.input_mode = InputMode::Normal;
        }
        // Clear the whole filter (matches the "C-u:clear" hint). TextArea's own
        // Ctrl+U is undo, so intercept it here before forwarding.
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.filter = SingleLineInput::new();
            app.item_cursor = 0;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::*;

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

    #[test]
    fn filter_esc_exits_mode() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::Filter;
        handle_key_filter(&mut app, make_key(KeyCode::Esc));
        assert!(matches!(app.input_mode, InputMode::Normal));
    }

    #[test]
    fn filter_tab_exits_mode_keeping_filter() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::Filter;
        app.filter = ta("fix");
        handle_key_filter(&mut app, make_key(KeyCode::Tab));
        assert!(matches!(app.input_mode, InputMode::Normal));
        // Tab leaves the field but keeps the filter text applied.
        assert_eq!(app.filter.value(), "fix");
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
    fn filter_enter_exits_mode_keeping_filter() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::Filter;
        app.filter = ta("fix");
        handle_key_filter(&mut app, make_key(KeyCode::Enter));
        assert!(matches!(app.input_mode, InputMode::Normal));
        // Enter leaves the field without inserting a newline.
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
