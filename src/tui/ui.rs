use crate::tui::{App, Focus, InputMode, LeftPaneEntry};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
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

    // Overlay modals on top if active
    if app.input_mode == InputMode::NewQuery {
        draw_new_query_modal(f, app, area);
    } else if app.input_mode == InputMode::NewFilterStreamName
        || app.input_mode == InputMode::NewFilterStreamFilter
    {
        draw_new_filter_stream_modal(f, app, area);
    } else if app.input_mode == InputMode::EditQueryName
        || app.input_mode == InputMode::EditQueryString
    {
        draw_edit_query_modal(f, app, area);
    } else if app.input_mode == InputMode::EditFilterStreamName
        || app.input_mode == InputMode::EditFilterStreamFilter
    {
        draw_edit_filter_stream_modal(f, app, area);
    }
}

fn draw_main(f: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(20),
            Constraint::Percentage(40),
            Constraint::Percentage(40),
        ])
        .split(area);

    draw_query_list(f, app, cols[0]);
    draw_item_list(f, app, cols[1]);
    draw_item_detail(f, app, cols[2]);
}

// ── Left pane: saved queries ─────────────────────────────────────────────────

fn draw_query_list(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::QueryList;
    let block = pane_block("Filter Streams", focused)
        .title_bottom(Line::from(" n:new  f:stream  e:edit  d:del ").right_aligned());

    let items: Vec<ListItem> = app
        .entries
        .iter()
        .map(|entry| match entry {
            LeftPaneEntry::Query(q) => {
                let kind_badge = if q.kind == "pull_request" { " PR " } else { " IS " };
                let label = format!("{kind_badge} {}", q.label);
                ListItem::new(label)
            }
            LeftPaneEntry::FilterStream(fs) => {
                // Indent with a visual tree connector
                let label = format!("   ↳ {}", fs.name);
                ListItem::new(label).style(Style::default().fg(Color::DarkGray))
            }
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(app.entry_cursor));

    let list = List::new(items)
        .block(block)
        .highlight_style(highlight_style(focused))
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, area, &mut state);
}

// ── Middle pane: item list ────────────────────────────────────────────────────

fn draw_item_list(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::ItemList;
    let filter_mode = app.input_mode == InputMode::Filter;

    // Split: filter bar (3 lines) + list
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(area);

    // Filter input bar
    let filter_label = if filter_mode {
        format!("/ {}_", app.filter)
    } else if app.filter.is_empty() {
        " /:filter".to_string()
    } else {
        format!("/ {}  (Esc:exit  C-u:clear)", app.filter)
    };
    let filter_style = if filter_mode {
        Style::default().fg(Color::Yellow)
    } else if !app.filter.is_empty() {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let filter_block = pane_block("", filter_mode);
    let filter_para = Paragraph::new(filter_label)
        .style(filter_style)
        .block(filter_block);
    f.render_widget(filter_para, split[0]);

    // Item list
    let filtered = app.filtered_items();
    let title = format!(
        "Items ({}/{})",
        filtered.len(),
        app.items.len()
    );
    let block = pane_block(&title, focused && !filter_mode);
    let filter_query = app.parsed_filter();
    let match_highlight = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let match_normal = Style::default().add_modifier(Modifier::BOLD);

    let items: Vec<ListItem> = filtered
        .iter()
        .map(|item| {
            let state_style = state_style(&item.state);
            let state_badge = state_badge(&item.state);
            let author = item.author.as_deref().unwrap_or("—");
            let title_spans =
                filter_query.highlight_spans(&item.title, match_normal, match_highlight);
            let mut spans = vec![
                Span::styled(state_badge, state_style),
                Span::raw(format!(" #{} ", item.number)),
            ];
            spans.extend(title_spans);
            spans.push(Span::raw(format!("  {author}")));
            ListItem::new(Line::from(spans))
        })
        .collect();

    let mut state = ListState::default();
    if !filtered.is_empty() {
        state.select(Some(app.item_cursor));
    }

    let list = List::new(items)
        .block(block)
        .highlight_style(highlight_style(focused))
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, split[1], &mut state);
}

// ── Right pane: item detail ───────────────────────────────────────────────────

fn draw_item_detail(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::ItemDetail;
    let block = pane_block("Detail", focused);

    let text = match app.selected_item() {
        None => vec![Line::from(Span::raw("No item selected"))],
        Some(item) => {
            let repo = format!("{}/{}", item.repo_owner, item.repo_name);
            let labels = if item.labels.is_empty() {
                "—".to_string()
            } else {
                item.labels.join(", ")
            };
            let author = item.author.clone().unwrap_or_else(|| "—".to_string());
            let state = item.state.clone();
            let title = item.title.clone();
            let updated_at = item.updated_at.clone();
            let url = item.url.clone();
            let number = item.number;
            let comment_count = item.comment_count;
            vec![
                Line::from(vec![
                    Span::styled(
                        format!("#{number} "),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(
                        title,
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::default(),
                Line::from(vec![
                    Span::styled("Repo:    ", Style::default().fg(Color::DarkGray)),
                    Span::raw(repo),
                ]),
                Line::from(vec![
                    Span::styled("Author:  ", Style::default().fg(Color::DarkGray)),
                    Span::raw(author),
                ]),
                Line::from(vec![
                    Span::styled("State:   ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        state.clone(),
                        state_style(&state),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("Updated: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(updated_at),
                ]),
                Line::from(vec![
                    Span::styled("Labels:  ", Style::default().fg(Color::DarkGray)),
                    Span::raw(labels),
                ]),
                Line::from(vec![
                    Span::styled("Comments:", Style::default().fg(Color::DarkGray)),
                    Span::raw(format!(" {comment_count}")),
                ]),
                Line::default(),
                Line::from(vec![
                    Span::styled("URL:     ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        url,
                        Style::default()
                            .fg(Color::Blue)
                            .add_modifier(Modifier::UNDERLINED),
                    ),
                ]),
            ]
        }
    };

    let para = Paragraph::new(text)
        .block(block)
        .wrap(Wrap { trim: false });

    f.render_widget(para, area);
}

// ── Status bar ────────────────────────────────────────────────────────────────

fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let mode_text = match app.input_mode {
        InputMode::Normal => match app.focus {
            Focus::QueryList => "QUERIES  Tab:focus  j/k:move  n:new query  f:new stream  e:edit  d:delete  q:quit",
            Focus::ItemList => "ITEMS    Tab:focus  j/k:move  /:filter  q:quit",
            Focus::ItemDetail => "DETAIL   Tab:focus  j/k:scroll  q:quit",
        },
        InputMode::Filter => "FILTER   Esc:exit  C-u:clear  state:open  author:name  label:bug  repo:owner/name",
        InputMode::NewQuery => "NEW QUERY  Enter:save  Esc:cancel",
        InputMode::NewFilterStreamName => "NEW STREAM (1/2: name)  Enter:next  Esc:cancel",
        InputMode::NewFilterStreamFilter => "NEW STREAM (2/2: filter)  Enter:save  Esc:cancel",
        InputMode::EditQueryName => "EDIT QUERY (1/2: name)  Enter:next  Esc:cancel",
        InputMode::EditQueryString => "EDIT QUERY (2/2: query)  Enter:save  Esc:cancel",
        InputMode::EditFilterStreamName => "EDIT STREAM (1/2: name)  Enter:next  Esc:cancel",
        InputMode::EditFilterStreamFilter => "EDIT STREAM (2/2: filter)  Enter:save  Esc:cancel",
    };

    let status = if let Some(msg) = &app.status {
        if app.syncing {
            format!(" {mode_text}  │  ⟳ Syncing…  │  {msg}")
        } else {
            format!(" {mode_text}  │  {msg}")
        }
    } else if app.syncing {
        format!(" {mode_text}  │  ⟳ Syncing…")
    } else {
        format!(" {mode_text}")
    };

    let para = Paragraph::new(status)
        .style(Style::default().bg(Color::DarkGray).fg(Color::White))
        .alignment(Alignment::Left);
    f.render_widget(para, area);
}

// ── New query modal ───────────────────────────────────────────────────────────

fn draw_new_query_modal(f: &mut Frame, app: &App, area: Rect) {
    let popup_area = centered_rect(60, 7, area);

    // Clear the background
    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(" New Query ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new("GitHub search query (e.g. repo:owner/name is:pr is:open)"),
        split[0],
    );
    f.render_widget(
        Paragraph::new(format!("> {}_", app.new_query_input))
            .style(Style::default().fg(Color::Yellow)),
        split[1],
    );
    f.render_widget(
        Paragraph::new("(PR kind is used by default)").style(
            Style::default().fg(Color::DarkGray),
        ),
        split[2],
    );
    f.render_widget(
        Paragraph::new("Enter:save  Esc:cancel").style(Style::default().fg(Color::DarkGray)),
        split[3],
    );
}

fn draw_new_filter_stream_modal(f: &mut Frame, app: &App, area: Rect) {
    let popup_area = centered_rect(60, 9, area);
    f.render_widget(Clear, popup_area);

    let (step, title) = if app.input_mode == InputMode::NewFilterStreamName {
        (1, " New Filter Stream — Step 1/2: Name ")
    } else {
        (2, " New Filter Stream — Step 2/2: Filter ")
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    if step == 1 {
        f.render_widget(Paragraph::new("Display name for this filter stream:"), split[0]);
        f.render_widget(
            Paragraph::new(format!("> {}_", app.new_filter_stream_name))
                .style(Style::default().fg(Color::Magenta)),
            split[1],
        );
        f.render_widget(
            Paragraph::new("Previously entered name shown above. Press Enter to continue.")
                .style(Style::default().fg(Color::DarkGray)),
            split[2],
        );
    } else {
        f.render_widget(
            Paragraph::new(format!("Name: {}", app.new_filter_stream_name))
                .style(Style::default().fg(Color::DarkGray)),
            split[0],
        );
        f.render_widget(
            Paragraph::new("Filter (e.g. state:open label:bug repo:owner/name):"),
            split[1],
        );
        f.render_widget(
            Paragraph::new(format!("> {}_", app.new_filter_stream_filter))
                .style(Style::default().fg(Color::Magenta)),
            split[2],
        );
    }
    f.render_widget(
        Paragraph::new("Enter:confirm  Esc:cancel").style(Style::default().fg(Color::DarkGray)),
        split[4],
    );
}

fn draw_edit_query_modal(f: &mut Frame, app: &App, area: Rect) {
    let popup_area = centered_rect(60, 9, area);
    f.render_widget(Clear, popup_area);

    let (step, title) = if app.input_mode == InputMode::EditQueryName {
        (1, " Edit Query — Step 1/2: Display Name ")
    } else {
        (2, " Edit Query — Step 2/2: GitHub Search Query ")
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    if step == 1 {
        f.render_widget(
            Paragraph::new("Display name (leave empty to use query string as label):"),
            split[0],
        );
        f.render_widget(
            Paragraph::new(format!("> {}_", app.edit_input))
                .style(Style::default().fg(Color::Cyan)),
            split[1],
        );
        f.render_widget(
            Paragraph::new("Press Enter to continue to query string")
                .style(Style::default().fg(Color::DarkGray)),
            split[2],
        );
    } else {
        f.render_widget(
            Paragraph::new(format!(
                "Name: {}",
                if app.edit_input.is_empty() { "(use query string)" } else { &app.edit_input }
            ))
            .style(Style::default().fg(Color::DarkGray)),
            split[0],
        );
        f.render_widget(
            Paragraph::new("GitHub search query (e.g. repo:owner/name is:pr is:open):"),
            split[1],
        );
        f.render_widget(
            Paragraph::new(format!("> {}_", app.edit_input2))
                .style(Style::default().fg(Color::Cyan)),
            split[2],
        );
        f.render_widget(
            Paragraph::new("(Saving will reset cache and re-sync)")
                .style(Style::default().fg(Color::DarkGray)),
            split[3],
        );
    }
    f.render_widget(
        Paragraph::new("Enter:confirm  Esc:cancel").style(Style::default().fg(Color::DarkGray)),
        split[4],
    );
}

fn draw_edit_filter_stream_modal(f: &mut Frame, app: &App, area: Rect) {
    let popup_area = centered_rect(60, 9, area);
    f.render_widget(Clear, popup_area);

    let (step, title) = if app.input_mode == InputMode::EditFilterStreamName {
        (1, " Edit Filter Stream — Step 1/2: Name ")
    } else {
        (2, " Edit Filter Stream — Step 2/2: Filter ")
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    if step == 1 {
        f.render_widget(Paragraph::new("Display name for this filter stream:"), split[0]);
        f.render_widget(
            Paragraph::new(format!("> {}_", app.edit_input))
                .style(Style::default().fg(Color::Cyan)),
            split[1],
        );
    } else {
        f.render_widget(
            Paragraph::new(format!("Name: {}", app.edit_input))
                .style(Style::default().fg(Color::DarkGray)),
            split[0],
        );
        f.render_widget(
            Paragraph::new("Filter (e.g. state:open label:bug repo:owner/name):"),
            split[1],
        );
        f.render_widget(
            Paragraph::new(format!("> {}_", app.edit_input2))
                .style(Style::default().fg(Color::Cyan)),
            split[2],
        );
    }
    f.render_widget(
        Paragraph::new("Enter:confirm  Esc:cancel").style(Style::default().fg(Color::DarkGray)),
        split[4],
    );
}

fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .border_style(border_style)
}

fn highlight_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .bg(Color::Cyan)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    }
}

fn state_style(state: &str) -> Style {
    match state {
        "open" => Style::default().fg(Color::Green),
        "merged" => Style::default().fg(Color::Magenta),
        "closed" => Style::default().fg(Color::Red),
        _ => Style::default(),
    }
}

fn state_badge(state: &str) -> &'static str {
    match state {
        "open" => "●",
        "merged" => "⬡",
        "closed" => "✕",
        _ => "?",
    }
}

/// Returns a centered `Rect` of fixed height and `percent_x` width.
fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let popup_width = area.width * percent_x / 100;
    let x = area.x + (area.width.saturating_sub(popup_width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, popup_width.min(area.width), height.min(area.height))
}
