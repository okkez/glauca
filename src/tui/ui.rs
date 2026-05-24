use crate::tui::{App, CommentEntry, Focus, InputMode, ItemAction, LeftPaneEntry, MergeStrategy};
use chrono::{DateTime, Local};
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
    } else if app.input_mode == InputMode::NewFilterStream {
        draw_new_filter_stream_modal(f, app, area);
    } else if app.input_mode == InputMode::EditQuery {
        draw_edit_query_modal(f, app, area);
    } else if app.input_mode == InputMode::EditFilterStream {
        draw_edit_filter_stream_modal(f, app, area);
    }

    if app.input_mode == InputMode::ActionMenu {
        draw_action_popup(f, app, area);
    } else if app.input_mode == InputMode::MergeMenu {
        draw_merge_menu_popup(f, app, area);
    } else if app.input_mode == InputMode::CommentsPopup {
        draw_comments_popup(f, app, area);
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
                let kind_badge = if q.kind == "pull_request" {
                    " PR "
                } else {
                    " IS "
                };
                let label = format!("{kind_badge} {}", q.label);
                ListItem::new(label)
            }
            LeftPaneEntry::FilterStream(fs) => {
                // Indent with a visual tree connector
                let label = format!("   ↳ {}", fs.name);
                ListItem::new(label).style(Style::default().fg(Color::Gray))
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
        Style::default().fg(Color::Gray)
    };
    let filter_block = pane_block("", filter_mode);
    let filter_para = Paragraph::new(filter_label)
        .style(filter_style)
        .block(filter_block);
    f.render_widget(filter_para, split[0]);

    // Item list
    let filtered = app.filtered_items();
    let title = format!("Items ({}/{})", filtered.len(), app.items.len());
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
            let kind_icon = kind_icon(&item.kind);
            let repo = format!("{}/{}", item.repo_owner, item.repo_name);
            let updated = format_local_datetime(&item.updated_at);
            let title_spans =
                filter_query.highlight_spans(&item.title, match_normal, match_highlight);

            // Line 1: state●  kind⎇  #number  title
            let mut line1_spans = vec![
                Span::styled(state_badge, state_style),
                Span::raw(" "),
                Span::styled(kind_icon, Style::default().fg(Color::Cyan)),
                Span::raw(format!(" #{} ", item.number)),
            ];
            line1_spans.extend(title_spans);

            // Line 2: (indent)  repo  ·  updated_at
            let line2 = Line::from(vec![
                Span::raw("      "),
                Span::styled(repo, Style::default().fg(Color::Gray)),
                Span::styled("  ·  ", Style::default().fg(Color::Gray)),
                Span::styled(updated, Style::default().fg(Color::Gray)),
            ]);

            ListItem::new(vec![Line::from(line1_spans), line2])
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
            let author = item.author.clone().unwrap_or_else(|| "—".to_string());
            let state = item.state.clone();
            let title = item.title.clone();
            let updated_at = format_local_datetime(&item.updated_at);
            let created_at = item
                .created_at_item
                .as_deref()
                .map(format_local_datetime)
                .unwrap_or_else(|| "—".to_string());
            let url = item.url.clone();
            let number = item.number;
            let comment_count = item.comment_count;
            let kind_icon = kind_icon(&item.kind);
            let is_pr = item.kind == "pull_request";

            let labels = if item.labels.is_empty() {
                "—".to_string()
            } else {
                item.labels.join(", ")
            };
            let assignees = if item.assignees.is_empty() {
                "—".to_string()
            } else {
                item.assignees.join(", ")
            };
            let milestone = item.milestone.clone().unwrap_or_else(|| "—".to_string());

            // Build combined reviewer list: submitted reviews + pending requests
            let reviewed_logins: std::collections::HashSet<&str> =
                item.reviews.iter().map(|(l, _)| l.as_str()).collect();
            let reviewer_spans: Vec<Span> = {
                let mut spans = Vec::new();
                for (login, state) in &item.reviews {
                    let (badge, style) = review_state_badge(state);
                    if !spans.is_empty() {
                        spans.push(Span::raw("  "));
                    }
                    spans.push(Span::styled(badge, style));
                    spans.push(Span::raw(format!(" {login}")));
                }
                for login in &item.requested_reviewers {
                    if !reviewed_logins.contains(login.as_str()) {
                        if !spans.is_empty() {
                            spans.push(Span::raw("  "));
                        }
                        spans.push(Span::styled("○", Style::default().fg(Color::Yellow)));
                        spans.push(Span::raw(format!(" {login}")));
                    }
                }
                if spans.is_empty() {
                    spans.push(Span::raw("—"));
                }
                spans
            };

            let mut lines = vec![
                // Title header
                Line::from(vec![
                    Span::styled(
                        format!("{kind_icon} #{number} "),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(title, Style::default().add_modifier(Modifier::BOLD)),
                ]),
                Line::default(),
                // Metadata block
                Line::from(vec![
                    Span::styled("Repo:     ", Style::default().fg(Color::Gray)),
                    Span::raw(repo),
                ]),
                Line::from(vec![
                    Span::styled("Author:   ", Style::default().fg(Color::Gray)),
                    Span::raw(author),
                ]),
                Line::from({
                    let mut spans = vec![
                        Span::styled("State:    ", Style::default().fg(Color::Gray)),
                        Span::styled(state_badge(&state), state_style(&state)),
                        Span::raw(format!(" {state}")),
                    ];
                    if is_pr && item.is_draft {
                        spans.push(Span::styled(
                            "  [Draft]",
                            Style::default().fg(Color::Yellow),
                        ));
                    }
                    spans
                }),
                Line::from(vec![
                    Span::styled("Created:  ", Style::default().fg(Color::Gray)),
                    Span::raw(created_at),
                ]),
                Line::from(vec![
                    Span::styled("Updated:  ", Style::default().fg(Color::Gray)),
                    Span::raw(updated_at),
                ]),
                Line::default(),
                Line::from(vec![
                    Span::styled("Labels:   ", Style::default().fg(Color::Gray)),
                    Span::raw(labels),
                ]),
                Line::from(vec![
                    Span::styled("Milestone:", Style::default().fg(Color::Gray)),
                    Span::raw(format!(" {milestone}")),
                ]),
                Line::from(vec![
                    Span::styled("Assignees:", Style::default().fg(Color::Gray)),
                    Span::raw(format!(" {assignees}")),
                ]),
                Line::from({
                    let mut spans = vec![
                        Span::styled("Reviewers:", Style::default().fg(Color::Gray)),
                        Span::raw(" "),
                    ];
                    spans.extend(reviewer_spans);
                    spans
                }),
                Line::from(vec![
                    Span::styled("Comments: ", Style::default().fg(Color::Gray)),
                    Span::raw(format!("{comment_count}")),
                ]),
            ];

            // PR-only fields
            if is_pr {
                if let (Some(base), Some(head)) = (&item.base_ref, &item.head_ref) {
                    lines.push(Line::from(vec![
                        Span::styled("Branch:   ", Style::default().fg(Color::Gray)),
                        Span::raw(format!("{head} → {base}")),
                    ]));
                }
                if let Some(rd) = &item.review_decision {
                    let (badge, style) = match rd.as_str() {
                        "APPROVED" => ("✓ APPROVED", Style::default().fg(Color::Green)),
                        "CHANGES_REQUESTED" => {
                            ("✗ CHANGES REQUESTED", Style::default().fg(Color::Red))
                        }
                        "REVIEW_REQUIRED" => {
                            ("○ REVIEW REQUIRED", Style::default().fg(Color::Yellow))
                        }
                        other => (other, Style::default()),
                    };
                    lines.push(Line::from(vec![
                        Span::styled("Review:   ", Style::default().fg(Color::Gray)),
                        Span::styled(badge, style),
                    ]));
                }
            }

            lines.push(Line::default());
            lines.push(Line::from(vec![
                Span::styled("URL:      ", Style::default().fg(Color::Gray)),
                Span::styled(
                    url,
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::UNDERLINED),
                ),
            ]));

            // Description body
            if let Some(body) = &item.body {
                if !body.is_empty() {
                    lines.push(Line::default());
                    lines.push(Line::from(Span::styled(
                        "─── Description ────────────────────────────────",
                        Style::default().fg(Color::DarkGray),
                    )));
                    for body_line in body.lines() {
                        lines.push(Line::from(Span::raw(body_line.to_string())));
                    }
                }
            }

            lines
        }
    };

    let para = Paragraph::new(text)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((app.detail_scroll, 0));

    f.render_widget(para, area);
}

// ── Status bar ────────────────────────────────────────────────────────────────

fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let enter_actions_hint = if app.selected_item().is_some()
        && matches!(app.focus, Focus::ItemList | Focus::ItemDetail)
    {
        "  Enter:actions"
    } else {
        ""
    };

    let mode_text = match app.input_mode {
        InputMode::Normal => match app.focus {
            Focus::QueryList => "QUERIES  h/l:pane  j/k:move  J/K:reorder  n:new query  f:new stream  e:edit  d:delete  q:quit".to_string(),
            Focus::ItemList => format!("ITEMS    h/l:pane  j/k:move  /:filter{enter_actions_hint}  q:quit"),
            Focus::ItemDetail => format!("DETAIL   h/l:pane  j/k:scroll{enter_actions_hint}  q:quit"),
        },
        InputMode::Filter => "FILTER   Esc:exit  C-u:clear  state:open  author:name  label:bug  repo:owner/name".to_string(),
        InputMode::NewQuery => "NEW QUERY  Tab:switch field  Enter:save  Esc:cancel".to_string(),
        InputMode::NewFilterStream => "NEW STREAM  Tab:switch field  Enter:save  Esc:cancel".to_string(),
        InputMode::EditQuery => "EDIT QUERY  Tab:switch field  Enter:save  Esc:cancel".to_string(),
        InputMode::EditFilterStream => "EDIT STREAM  Tab:switch field  Enter:save  Esc:cancel".to_string(),
        InputMode::ActionMenu => "ACTIONS  j/k:move  Enter:confirm  Esc:cancel".to_string(),
        InputMode::MergeMenu => "MERGE    j/k:move  Enter:confirm  Esc:back".to_string(),
        InputMode::CommentsPopup => "COMMENTS  j/k:scroll  g/G:top/bottom  Esc/q:close".to_string(),
    };

    let status = if let Some(msg) = &app.status {
        if app.syncing && app.bg_sync_pending > 0 {
            format!(
                " {mode_text}  │  ⟳ Syncing…  │  ⟳ Auto ({})  │  {msg}",
                app.bg_sync_pending
            )
        } else if app.syncing {
            format!(" {mode_text}  │  ⟳ Syncing…  │  {msg}")
        } else if app.bg_sync_pending > 0 {
            format!(
                " {mode_text}  │  ⟳ Auto ({})  │  {msg}",
                app.bg_sync_pending
            )
        } else {
            format!(" {mode_text}  │  {msg}")
        }
    } else if app.syncing && app.bg_sync_pending > 0 {
        format!(
            " {mode_text}  │  ⟳ Syncing…  │  ⟳ Auto ({})",
            app.bg_sync_pending
        )
    } else if app.syncing {
        format!(" {mode_text}  │  ⟳ Syncing…")
    } else if app.bg_sync_pending > 0 {
        format!(" {mode_text}  │  ⟳ Auto ({})", app.bg_sync_pending)
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
    let popup_area = centered_rect(60, 9, area);
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
            Constraint::Length(1),
        ])
        .split(inner);

    let name_style = if app.modal_field == 0 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Gray)
    };
    let query_style = if app.modal_field == 1 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Gray)
    };

    f.render_widget(
        Paragraph::new("Display name (optional — leave blank to use query):"),
        split[0],
    );
    f.render_widget(
        Paragraph::new(format!(
            "> {}{}",
            app.new_query_name,
            if app.modal_field == 0 { "_" } else { "" }
        ))
        .style(name_style),
        split[1],
    );
    f.render_widget(
        Paragraph::new("GitHub search query (e.g. repo:owner/name is:pr is:open):"),
        split[2],
    );
    f.render_widget(
        Paragraph::new(format!(
            "> {}{}",
            app.new_query_input,
            if app.modal_field == 1 { "_" } else { "" }
        ))
        .style(query_style),
        split[3],
    );
    f.render_widget(
        Paragraph::new("Tab:switch  Enter:save  Esc:cancel")
            .style(Style::default().fg(Color::Gray)),
        split[4],
    );
}

fn draw_new_filter_stream_modal(f: &mut Frame, app: &App, area: Rect) {
    let popup_area = centered_rect(60, 9, area);
    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(" New Filter Stream ")
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

    let name_style = if app.modal_field == 0 {
        Style::default().fg(Color::Magenta)
    } else {
        Style::default().fg(Color::Gray)
    };
    let filter_style = if app.modal_field == 1 {
        Style::default().fg(Color::Magenta)
    } else {
        Style::default().fg(Color::Gray)
    };

    f.render_widget(Paragraph::new("Display name:"), split[0]);
    f.render_widget(
        Paragraph::new(format!(
            "> {}{}",
            app.new_filter_stream_name,
            if app.modal_field == 0 { "_" } else { "" }
        ))
        .style(name_style),
        split[1],
    );
    f.render_widget(
        Paragraph::new("Filter (e.g. state:open label:bug):"),
        split[2],
    );
    f.render_widget(
        Paragraph::new(format!(
            "> {}{}",
            app.new_filter_stream_filter,
            if app.modal_field == 1 { "_" } else { "" }
        ))
        .style(filter_style),
        split[3],
    );
    f.render_widget(
        Paragraph::new("Tab:switch  Enter:save  Esc:cancel")
            .style(Style::default().fg(Color::Gray)),
        split[4],
    );
}

fn draw_edit_query_modal(f: &mut Frame, app: &App, area: Rect) {
    let popup_area = centered_rect(60, 9, area);
    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(" Edit Query ")
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

    let name_style = if app.modal_field == 0 {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::Gray)
    };
    let query_style = if app.modal_field == 1 {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::Gray)
    };

    f.render_widget(
        Paragraph::new("Display name (empty = use query string as label):"),
        split[0],
    );
    f.render_widget(
        Paragraph::new(format!(
            "> {}{}",
            app.edit_input,
            if app.modal_field == 0 { "_" } else { "" }
        ))
        .style(name_style),
        split[1],
    );
    f.render_widget(Paragraph::new("GitHub search query:"), split[2]);
    f.render_widget(
        Paragraph::new(format!(
            "> {}{}",
            app.edit_input2,
            if app.modal_field == 1 { "_" } else { "" }
        ))
        .style(query_style),
        split[3],
    );
    f.render_widget(
        Paragraph::new("Tab:switch  Enter:save  Esc:cancel")
            .style(Style::default().fg(Color::Gray)),
        split[4],
    );
}

fn draw_edit_filter_stream_modal(f: &mut Frame, app: &App, area: Rect) {
    let popup_area = centered_rect(60, 9, area);
    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(" Edit Filter Stream ")
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

    let name_style = if app.modal_field == 0 {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::Gray)
    };
    let filter_style = if app.modal_field == 1 {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::Gray)
    };

    f.render_widget(Paragraph::new("Display name:"), split[0]);
    f.render_widget(
        Paragraph::new(format!(
            "> {}{}",
            app.edit_input,
            if app.modal_field == 0 { "_" } else { "" }
        ))
        .style(name_style),
        split[1],
    );
    f.render_widget(
        Paragraph::new("Filter (e.g. state:open label:bug repo:owner/name):"),
        split[2],
    );
    f.render_widget(
        Paragraph::new(format!(
            "> {}{}",
            app.edit_input2,
            if app.modal_field == 1 { "_" } else { "" }
        ))
        .style(filter_style),
        split[3],
    );
    f.render_widget(
        Paragraph::new("Tab:switch  Enter:save  Esc:cancel")
            .style(Style::default().fg(Color::Gray)),
        split[4],
    );
}

fn draw_action_popup(f: &mut Frame, app: &App, area: Rect) {
    let item = match app.selected_item() {
        Some(item) => item,
        None => return,
    };
    let actions = ItemAction::available_for(&item.kind);
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

fn draw_merge_menu_popup(f: &mut Frame, app: &App, area: Rect) {
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

fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::Gray)
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

fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "pull_request" => "⎇",
        _ => "○", // issue
    }
}

/// Returns (badge_str, style) for a reviewer's submitted review state.
fn review_state_badge(state: &str) -> (&'static str, Style) {
    match state {
        "APPROVED" => ("✅", Style::default().fg(Color::Green)),
        "CHANGES_REQUESTED" => ("✗", Style::default().fg(Color::Red)),
        "COMMENTED" => ("💬", Style::default().fg(Color::Blue)),
        "DISMISSED" => ("↩", Style::default().fg(Color::Cyan)),
        _ => ("?", Style::default()),
    }
}

/// Parse a RFC3339 UTC string and format it as local time `YYYY-MM-DD HH:MM`.
fn format_local_datetime(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| {
            let local: DateTime<Local> = dt.with_timezone(&Local);
            local.format("%Y-%m-%d %H:%M").to_string()
        })
        .unwrap_or_else(|_| {
            // Fall back: strip trailing Z/offset if present, return as-is trimmed
            s.get(..16).unwrap_or(s).replace('T', " ")
        })
}

/// Returns a centered `Rect` of fixed height and `percent_x` width.
fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let popup_width = area.width * percent_x / 100;
    let x = area.x + (area.width.saturating_sub(popup_width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, popup_width.min(area.width), height.min(area.height))
}

fn centered_rect_fixed(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

// ── Comments popup ────────────────────────────────────────────────────────────

fn draw_comments_popup(f: &mut Frame, app: &App, area: Rect) {
    // Cover the right 60 % of the screen (overlaps the detail pane)
    let width = (area.width * 60 / 100).max(50).min(area.width);
    let height = (area.height * 85 / 100).max(10).min(area.height);
    let x = area.x + area.width.saturating_sub(width);
    let y = area.y + area.height.saturating_sub(height) / 2;
    let popup_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, popup_area);

    let item_title = app
        .selected_item()
        .map(|i| format!(" Comments — #{} {} ", i.number, i.title))
        .unwrap_or_else(|| " Comments ".into());

    let block = Block::default().borders(Borders::ALL).title(item_title);
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    // Split inner: content + hint
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    if app.comments_loading {
        let loading = Paragraph::new("Loading comments…")
            .style(Style::default().fg(Color::Yellow))
            .alignment(Alignment::Center);
        f.render_widget(loading, chunks[0]);
    } else if app.comments.is_empty() {
        let empty = Paragraph::new("No comments.")
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center);
        f.render_widget(empty, chunks[0]);
    } else {
        let lines = build_comment_lines(&app.comments, chunks[0].width as usize);
        let total = lines.len();
        // Clamp scroll
        let max_scroll = total.saturating_sub(chunks[0].height as usize);
        let scroll = app.comments_scroll.min(max_scroll);

        let para = Paragraph::new(lines)
            .scroll((scroll as u16, 0))
            .wrap(Wrap { trim: false });
        f.render_widget(para, chunks[0]);

        // Scroll indicator
        let indicator = if total > chunks[0].height as usize {
            format!("↑↓ scroll  {}/{} lines", scroll + chunks[0].height as usize, total)
        } else {
            String::new()
        };
        let hint = Paragraph::new(
            Line::from(vec![
                Span::styled(
                    "Esc/q: close  j/k: scroll  g/G: top/bottom  ",
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(indicator, Style::default().fg(Color::DarkGray)),
            ])
        )
        .alignment(Alignment::Center);
        f.render_widget(hint, chunks[1]);
    }
}

fn build_comment_lines<'a>(comments: &'a [CommentEntry], width: usize) -> Vec<Line<'a>> {
    let mut lines: Vec<Line<'a>> = Vec::new();
    let sep_width = width.max(4) - 4; // account for block padding
    for (i, c) in comments.iter().enumerate() {
        if i > 0 {
            // Bold separator with comment number
            lines.push(Line::from(Span::styled(
                "━".repeat(sep_width),
                Style::default().fg(Color::Yellow),
            )));
            lines.push(Line::from(""));
        }
        // Header line: ▌ @author  2026-05-24 20:15
        lines.push(Line::from(vec![
            Span::styled("▌ ", Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("@{}", c.author),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if c.created_at.is_empty() {
                    String::new()
                } else {
                    format!("   🕐 {}", c.created_at)
                },
                Style::default().fg(Color::White),
            ),
        ]));
        lines.push(Line::from(""));
        // Body (each line as a separate Line)
        for body_line in c.body.lines() {
            lines.push(Line::from(Span::raw(body_line.to_string())));
        }
        lines.push(Line::from(""));
    }
    lines
}
