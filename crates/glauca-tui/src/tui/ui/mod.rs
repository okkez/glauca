//! Ratatui rendering for the TUI. This module owns the top-level frame
//! (`draw` entry point, the three-pane layout, and the status bar) and the
//! shared imports; the panes, detail view, modals/popups, comments overlay,
//! and text/layout helpers live in the submodules.

use crate::tui::icons::Icons;
use crate::tui::single_line_input::SingleLineInput;
use crate::tui::{
    App, CommentEntry, Focus, InputMode, LeftPaneEntry, MergeStrategy, item_actions,
    modal_fields_ref,
};
use chrono::Utc;
use glauca_core::engine::ReviewEvent;
use glauca_core::filter::FilterQuery;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

mod comments;
mod detail;
mod modals;
mod panes;
mod text;
mod widgets;

use comments::draw_comments_popup;
use detail::draw_item_detail;
use modals::{
    draw_action_popup, draw_custom_action_popup, draw_help_popup, draw_merge_menu_popup,
    draw_modal, draw_review_menu_popup,
};
use panes::{draw_item_list, draw_query_list};
use text::{highlight_spans, wrap_spans};
use widgets::{
    centered_rect, centered_rect_fixed, draw_prompted_field, highlight_style, highlight_window,
    pane_block, state_style,
};

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();

    // Split into status bar (1 line) + main content
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    draw_main(f, app, root[0]);
    draw_status_bar(f, app, root[1]);

    // Overlay the two-field input modal on top if one is active (no-op otherwise).
    draw_modal(f, app, area);

    if app.input_mode == InputMode::ActionMenu {
        draw_action_popup(f, app, area);
    } else if app.input_mode == InputMode::CustomActionMenu {
        draw_custom_action_popup(f, app, area);
    } else if app.input_mode == InputMode::MergeMenu {
        draw_merge_menu_popup(f, app, area);
    } else if app.input_mode == InputMode::ReviewMenu {
        draw_review_menu_popup(f, app, area);
    } else if app.input_mode == InputMode::CommentsPopup {
        draw_comments_popup(f, app, area);
    } else if app.input_mode == InputMode::Help {
        draw_help_popup(f, area);
    }
}

fn draw_main(f: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(20),
            Constraint::Percentage(30),
            Constraint::Percentage(50),
        ])
        .split(area);

    draw_query_list(f, app, cols[0]);
    draw_item_list(f, app, cols[1]);
    draw_item_detail(f, app, cols[2]);
}

// ── Status bar ────────────────────────────────────────────────────────────────

fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let on_item =
        app.selected_item().is_some() && matches!(app.focus, Focus::ItemList | Focus::ItemDetail);
    let enter_actions_hint = if on_item { "  Enter:actions" } else { "" };
    let refresh_hint = if on_item { "  r:refresh" } else { "" };
    // octorus review is PR-only (and `on_item` already requires item focus).
    let selected_is_pr = app
        .selected_item()
        .map(|i| i.kind == "pull_request")
        .unwrap_or(false);
    let review_hint = if on_item && selected_is_pr {
        "  R:review"
    } else {
        ""
    };
    // Only hint `x` when the selected item actually has an applicable action.
    let custom_hint = if on_item && app.has_custom_actions_for_selected() {
        "  x:actions"
    } else {
        ""
    };

    let mode_text = match app.input_mode {
        InputMode::Normal => match app.focus {
            Focus::QueryList => "QUERIES  h/l:pane  j/k:move  J/K:reorder  n:new query  f:new stream  e:edit  d:delete  r:refresh  a:mark all read  ?:help  q:quit".to_string(),
            Focus::ItemList => format!("ITEMS    h/l:pane  j/k:move  /:filter{enter_actions_hint}{refresh_hint}{review_hint}{custom_hint}  ?:help  q:quit"),
            Focus::ItemDetail => format!("DETAIL   h/l:pane  j/k:scroll{enter_actions_hint}{refresh_hint}{review_hint}{custom_hint}  ?:help  q:quit"),
        },
        InputMode::Filter => "FILTER   Esc:exit  C-u:clear  is:pr  state:open  author:name  label:bug  repo:owner/name".to_string(),
        InputMode::NewQuery => "NEW QUERY  Tab:switch field  Enter:save  Esc:cancel".to_string(),
        InputMode::NewFilterStream => "NEW STREAM  Tab:switch field  Enter:save  Esc:cancel".to_string(),
        InputMode::EditQuery => "EDIT QUERY  Tab:switch field  Enter:save  Esc:cancel".to_string(),
        InputMode::EditFilterStream => "EDIT STREAM  Tab:switch field  Enter:save  Esc:cancel".to_string(),
        InputMode::ActionMenu => "ACTIONS  j/k:move  Enter:confirm  Esc:cancel".to_string(),
        InputMode::CustomActionMenu => {
            "CUSTOM   j/k:move  Enter:run  Esc:cancel".to_string()
        }
        InputMode::MergeMenu => "MERGE    j/k:move  Enter:confirm  Esc:back".to_string(),
        InputMode::ReviewMenu => "REVIEW   j/k:move  Enter:submit  Esc:cancel".to_string(),
        InputMode::CommentsPopup => "COMMENTS  j/k:scroll  g/G:top/bottom  Esc/q:close".to_string(),
        InputMode::Help => "HELP     Esc/?/q:close".to_string(),
    };

    // Assemble only the segments that apply, in fixed order, then join — instead
    // of enumerating every on/off combination of syncing / pending / status.
    let mut segments = vec![mode_text];
    if app.syncing {
        segments.push(format!("{} Syncing…", app.icons.syncing));
    }
    if app.bg_sync_pending > 0 {
        segments.push(format!(
            "{} Auto ({})",
            app.icons.syncing, app.bg_sync_pending
        ));
    }
    if app.notifications_enabled {
        segments.push(app.icons.bell.to_string());
    }
    // Present only while the icon-font set is active, so it only shows where it renders.
    if let Some(badge) = app.icons.mode_badge {
        segments.push(badge.to_string());
    }
    if let Some(msg) = &app.status {
        segments.push(msg.clone());
    }
    let status = format!(" {}", segments.join("  │  "));

    let para = Paragraph::new(status)
        .style(Style::default().bg(Color::DarkGray).fg(Color::White))
        .alignment(Alignment::Left);
    f.render_widget(para, area);
}
