//! Overlays: the two-field input modal (new/edit query & filter stream) and the
//! action / custom-action / help / merge / review popup menus.

use super::*;

/// Draw a centered two-field input modal (a name field plus a second field).
/// `border_color` tints the border and the active field's label. Each field is
/// an editable `TextArea` that renders its own cursor (visible only on the
/// active field; see `sync_modal_cursors`). Shared by the new/edit query and
/// filter-stream modals, which differ only in these strings.
fn draw_two_field_modal(
    f: &mut Frame,
    area: Rect,
    title: &str,
    border_color: Color,
    fields: [(&str, &SingleLineInput); 2],
    active_field: usize,
) {
    let popup_area = centered_rect(60, 9, area);
    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    // Five one-line rows: [label0, input0, label1, input1, key hint].
    // Field i occupies rows i*2 (label) and i*2+1 (input); row 4 is the hint.
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1); 5])
        .split(inner);

    for (i, (label, value)) in fields.iter().enumerate() {
        let active = active_field == i;
        let label_style = if active {
            Style::default().fg(border_color)
        } else {
            Style::default().fg(Color::Gray)
        };
        f.render_widget(Paragraph::new(*label).style(label_style), split[i * 2]);
        draw_prompted_field(f, split[i * 2 + 1], "> ", label_style, value);
    }
    f.render_widget(
        Paragraph::new("Tab:switch  Enter:save  Esc:cancel")
            .style(Style::default().fg(Color::Gray)),
        split[4],
    );
}

/// Draw the active two-field input modal (new/edit × query/filter stream). The
/// title, border color and field labels are selected per `input_mode`; the two
/// `TextArea` fields come from `modal_fields_ref`. No-op outside those modals.
pub(super) fn draw_modal(f: &mut Frame, app: &App, area: Rect) {
    let (title, color, labels): (&str, Color, [&str; 2]) = match app.input_mode {
        InputMode::NewQuery => (
            " New Query ",
            Color::Yellow,
            [
                "Display name (optional — leave blank to use query):",
                "GitHub search query (e.g. repo:owner/name is:pr is:open):",
            ],
        ),
        InputMode::NewFilterStream => (
            " New Filter Stream ",
            Color::Magenta,
            [
                "Display name:",
                "Filter (e.g. is:pr is:draft assignee:name label:bug):",
            ],
        ),
        InputMode::EditQuery => (
            " Edit Query ",
            Color::Cyan,
            [
                "Display name (empty = use query string as label):",
                "GitHub search query:",
            ],
        ),
        InputMode::EditFilterStream => (
            " Edit Filter Stream ",
            Color::Cyan,
            [
                "Display name:",
                "Filter (e.g. is:pr assignee:name milestone:v2 repo:owner/name):",
            ],
        ),
        _ => return,
    };
    let Some((f0, f1)) = modal_fields_ref(app) else {
        return;
    };
    draw_two_field_modal(
        f,
        area,
        title,
        color,
        [(labels[0], f0), (labels[1], f1)],
        app.modal_field,
    );
}

pub(super) fn draw_action_popup(f: &mut Frame, app: &App, area: Rect) {
    let item = match app.selected_item() {
        Some(item) => item,
        None => return,
    };
    let actions = item_actions(&item.kind);
    let popup_area = centered_rect_fixed(40, actions.len() as u16 + 3, area);

    f.render_widget(Clear, popup_area);

    let block = Block::default().borders(Borders::ALL).title(" Actions ");
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let items: Vec<ListItem> = actions
        .iter()
        .enumerate()
        .map(|(i, action)| {
            if i == app.action_cursor {
                ListItem::new(format!(" ▶ {} ", action.label())).style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ListItem::new(format!("   {} ", action.label()))
            }
        })
        .collect();

    f.render_widget(List::new(items), chunks[0]);
    f.render_widget(
        Paragraph::new("j/k: move  Enter: confirm  Esc: cancel")
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center),
        chunks[1],
    );
}

/// Custom-action picker (opened with `x`). Lists the user-defined actions that
/// apply to the selected item's kind; Enter runs the highlighted one.
pub(super) fn draw_custom_action_popup(f: &mut Frame, app: &App, area: Rect) {
    let actions = app.custom_actions_for_selected();
    if actions.is_empty() {
        return;
    }
    let popup_area = centered_rect_fixed(48, actions.len() as u16 + 3, area);

    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Custom actions ");
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let items: Vec<ListItem> = actions
        .iter()
        .enumerate()
        .map(|(i, action)| {
            if i == app.custom_action_cursor {
                ListItem::new(format!(" ▶ {} ", action.display_label())).style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ListItem::new(format!("   {} ", action.display_label()))
            }
        })
        .collect();

    f.render_widget(List::new(items), chunks[0]);
    f.render_widget(
        Paragraph::new("j/k: move  Enter: run  Esc: cancel")
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center),
        chunks[1],
    );
}

/// Keybinding cheat-sheet overlay (opened with `?`). Static two-column list so it
/// fits without scrolling; closed with Esc / `?` / `q` (see `handle_key_help`).
pub(super) fn draw_help_popup(f: &mut Frame, area: Rect) {
    let header = |text: &str| {
        Line::from(Span::styled(
            text.to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
    };
    let entry = |key: &str, desc: &str| {
        Line::from(vec![
            Span::styled(format!("  {key:<9}"), Style::default().fg(Color::Yellow)),
            Span::raw(desc.to_string()),
        ])
    };

    let left = vec![
        header("Global"),
        entry("?", "this help"),
        entry("q", "quit"),
        entry("h l ← →", "move focus"),
        entry("Tab S-Tab", "cycle panes"),
        entry("S", "full resync (+prune)"),
        entry("u", "apply pending updates"),
        entry("N", "toggle desktop notifications"),
        entry("F", "toggle icon font"),
        Line::raw(""),
        header("Query list"),
        entry("j k", "move selection"),
        entry("n", "new query"),
        entry("f", "new filter stream"),
        entry("e", "edit entry"),
        entry("d", "delete entry"),
        entry("a", "mark all read"),
        entry("J K", "reorder"),
        entry("r", "re-sync query"),
    ];
    let right = vec![
        header("Items / detail"),
        entry("j k", "move / scroll"),
        entry("/", "filter (item list)"),
        entry("Enter", "actions menu"),
        entry("o", "open in browser"),
        entry("y", "copy URL"),
        entry("x", "custom actions"),
        entry("r", "refresh item"),
        entry("R", "review w/ octorus (PR)"),
    ];

    // Height: tallest column + borders (2) + hint line (1).
    let height = left.len().max(right.len()) as u16 + 3;
    let popup_area = centered_rect_fixed(64, height, area);
    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Keybindings ");
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);

    f.render_widget(Paragraph::new(left), cols[0]);
    f.render_widget(Paragraph::new(right), cols[1]);
    f.render_widget(
        Paragraph::new("Esc / ? / q: close")
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center),
        rows[1],
    );
}

pub(super) fn draw_merge_menu_popup(f: &mut Frame, app: &App, area: Rect) {
    let strategies = MergeStrategy::all();
    let popup_area = centered_rect_fixed(40, strategies.len() as u16 + 3, area);

    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Merge Strategy ");
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let items: Vec<ListItem> = strategies
        .iter()
        .enumerate()
        .map(|(i, strategy)| {
            if i == app.merge_strategy_cursor {
                ListItem::new(format!(" ▶ {} ", strategy.label())).style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ListItem::new(format!("   {} ", strategy.label()))
            }
        })
        .collect();

    f.render_widget(List::new(items), chunks[0]);
    f.render_widget(
        Paragraph::new("j/k: move  Enter: confirm  Esc: back")
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center),
        chunks[1],
    );
}

pub(super) fn draw_review_menu_popup(f: &mut Frame, app: &App, area: Rect) {
    let events = ReviewEvent::all();
    let popup_area = centered_rect_fixed(40, events.len() as u16 + 3, area);

    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Submit Review ");
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let items: Vec<ListItem> = events
        .iter()
        .enumerate()
        .map(|(i, event)| {
            if i == app.review_event_cursor {
                ListItem::new(format!(" ▶ {} ", event.label())).style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ListItem::new(format!("   {} ", event.label()))
            }
        })
        .collect();

    f.render_widget(List::new(items), chunks[0]);
    f.render_widget(
        Paragraph::new("j/k: move  Enter: submit  Esc: cancel")
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center),
        chunks[1],
    );
}
