use crate::tui::icons::Icons;
use crate::tui::{App, CommentEntry, Focus, InputMode, LeftPaneEntry, MergeStrategy, item_actions};
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
use unicode_width::UnicodeWidthChar;

/// Selection marker drawn in the item list's left gutter (reserved on every row).
const HIGHLIGHT_SYMBOL: &str = "▶ ";

/// Item-list left-gutter layout, in display columns. The unread marker and the
/// non-unread blank are both `MARKER_CELL_W` wide so item icons line up across
/// rows. `ROW_INDENT_W` is a fixed approximation of line 1's prefix width that
/// keeps the line-2 (repo) column aligned across rows — intentionally not the
/// per-row `prefix_w`, which shifts with the number's digit count. Keep both in
/// sync with the prefix layout in `draw_item_list`.
const MARKER_CELL_W: usize = 3;
const ROW_INDENT_W: usize = 10;

/// Build styled spans for `text`, highlighting the earliest filter-token match
/// (range computed by `FilterQuery::highlight_range`).
fn highlight_spans<'a>(
    query: &FilterQuery,
    text: &'a str,
    normal: Style,
    highlight: Style,
) -> Vec<Span<'a>> {
    match query.highlight_range(text) {
        None => vec![Span::styled(text, normal)],
        Some((start, end)) => {
            let mut spans = Vec::new();
            if start > 0 {
                spans.push(Span::styled(&text[..start], normal));
            }
            spans.push(Span::styled(&text[start..end], highlight));
            if end < text.len() {
                spans.push(Span::styled(&text[end..], normal));
            }
            spans
        }
    }
}

/// Wrap styled `spans` (e.g. a title's highlight fragments) to `max_cols`
/// display columns, returning one span vector per visual line. Breaks on the
/// last whitespace that fits (word wrap); a single word wider than `max_cols`
/// is hard-broken at the column limit. Display width is measured with
/// unicode-width so CJK (full-width) characters count as two columns. Each
/// character keeps its original span style.
fn wrap_spans(spans: &[Span], max_cols: usize) -> Vec<Vec<Span<'static>>> {
    let max_cols = max_cols.max(1);

    // Flatten into (char, style) so we can re-break independently of the
    // original fragment boundaries while preserving per-character styling.
    let chars: Vec<(char, Style)> = spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .collect();

    let mut lines: Vec<Vec<(char, Style)>> = Vec::new();
    let mut cur: Vec<(char, Style)> = Vec::new();
    let mut cur_cols = 0usize;
    // Column index (into `cur`) just after the last whitespace, i.e. where the
    // next line would resume if we break on a word boundary.
    let mut last_break: Option<usize> = None;

    for (c, style) in chars {
        let char_cols = UnicodeWidthChar::width(c).unwrap_or(0);
        if cur_cols + char_cols > max_cols && !cur.is_empty() {
            // A space that overflows is the word boundary itself: end the line
            // here and consume the space (no leading space on the next line).
            if c == ' ' {
                lines.push(std::mem::take(&mut cur));
                cur_cols = 0;
                last_break = None;
                continue;
            }
            match last_break {
                // Break at the last whitespace: carry the trailing word to the
                // next line, dropping the breaking space.
                Some(brk) if brk > 0 && brk < cur.len() => {
                    let carry: Vec<(char, Style)> = cur.split_off(brk);
                    if cur.last().map(|(c, _)| *c) == Some(' ') {
                        cur.pop();
                    }
                    lines.push(std::mem::take(&mut cur));
                    cur_cols = carry
                        .iter()
                        .map(|(c, _)| UnicodeWidthChar::width(*c).unwrap_or(0))
                        .sum();
                    cur = carry;
                }
                // No usable break point: hard-break before this char.
                _ => {
                    lines.push(std::mem::take(&mut cur));
                    cur_cols = 0;
                }
            }
            last_break = None;
        }
        if c == ' ' {
            last_break = Some(cur.len() + 1);
        }
        cur.push((c, style));
        cur_cols += char_cols;
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }

    // Coalesce consecutive same-style chars back into owned spans.
    lines
        .into_iter()
        .map(|line| {
            let mut out: Vec<Span<'static>> = Vec::new();
            for (c, style) in line {
                match out.last_mut() {
                    Some(last) if last.style == style => last.content.to_mut().push(c),
                    _ => out.push(Span::styled(c.to_string(), style)),
                }
            }
            out
        })
        .collect()
}

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

// ── Left pane: saved queries ─────────────────────────────────────────────────

fn draw_query_list(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::QueryList;
    let block = pane_block("Filter Streams", focused);

    let items: Vec<ListItem> = app
        .entries
        .iter()
        .map(|entry| {
            let unread = app
                .unread_counts
                .get(&entry.unread_key())
                .copied()
                .unwrap_or(0);
            let badge = (unread > 0).then(|| {
                Span::styled(
                    format!(" ({unread})"),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
            });

            match entry {
                LeftPaneEntry::Query(q) => {
                    let mut spans = vec![Span::raw(format!("{} {}", app.icons.search, q.label))];
                    if let Some(badge) = badge {
                        spans.push(badge);
                    }
                    ListItem::new(Line::from(spans))
                }
                LeftPaneEntry::FilterStream(fs) => {
                    let mut spans = vec![Span::styled(
                        format!("   ↳ {}", fs.name),
                        Style::default().fg(Color::Gray),
                    )];
                    if let Some(badge) = badge {
                        spans.push(badge);
                    }
                    ListItem::new(Line::from(spans))
                }
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

    // Layout: optional "N updated" banner (1 line) + filter bar (3 lines) + list.
    // The banner shows when a background sync brought results we held back; press
    // `u` to apply them.
    let has_banner = app.pending_count > 0;
    let split = if has_banner {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Min(1),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(area)
    };
    let (filter_area, list_area) = if has_banner {
        let banner = Paragraph::new(format!(
            "{} {} updated — press u to refresh",
            app.icons.refresh, app.pending_count
        ))
        .style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
        f.render_widget(banner, split[0]);
        (split[1], split[2])
    } else {
        (split[0], split[1])
    };

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
    f.render_widget(filter_para, filter_area);

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

    // Width available for a row's content: list width minus the block borders
    // (left+right) and the highlight-symbol gutter ratatui reserves on every row.
    let symbol_w: usize = HIGHLIGHT_SYMBOL
        .chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum();
    let inner_w = (list_area.width as usize).saturating_sub(2 + symbol_w);

    // Sample the clock once for the whole list so each row's relative time is
    // measured against the same instant (and we avoid a per-row `Utc::now()`).
    let now = Utc::now();
    let items: Vec<ListItem> = filtered
        .iter()
        .map(|item| {
            let item_style = state_style(&item.state);
            let item_icon = app.icons.item_icon(&item.kind, &item.state);
            let repo = format!("{}/{}", item.repo_owner, item.repo_name);
            let updated = glauca_core::time::format_relative_time_since(&item.updated_at, now);
            let title_spans =
                highlight_spans(&filter_query, &item.title, match_normal, match_highlight);

            // Line 1 prefix: "new-marker  item-icon  #number", then the title
            // (appended, wrapped below). The item icon is one glyph encoding the
            // kind (issue/PR/merge), coloured by state. Both marker branches are
            // `MARKER_CELL_W` wide (see its doc) so the icons line up; kept as
            // separate spans so we can measure the prefix width and indent the
            // wrapped title continuation lines to match.
            let mut prefix_spans = vec![if item.is_new {
                Span::styled(
                    format!("{}{}", app.icons.new_item, " ".repeat(MARKER_CELL_W - 1)),
                    Style::default().fg(Color::Yellow),
                )
            } else {
                Span::raw(" ".repeat(MARKER_CELL_W))
            }];
            prefix_spans.extend([
                Span::styled(item_icon, item_style),
                Span::raw(format!("  #{} ", item.number)),
            ]);
            let prefix_w: usize = prefix_spans.iter().map(Span::width).sum();

            // Wrap the title into the remaining width so long titles show in
            // full across multiple lines instead of being truncated.
            let title_w = inner_w.saturating_sub(prefix_w).max(8);
            let wrapped = wrap_spans(&title_spans, title_w);

            let mut lines: Vec<Line> = Vec::with_capacity(wrapped.len() + 1);
            let indent = " ".repeat(prefix_w);
            for (i, title_line) in wrapped.into_iter().enumerate() {
                let mut spans = if i == 0 {
                    prefix_spans.clone()
                } else {
                    vec![Span::raw(indent.clone())]
                };
                spans.extend(title_line);
                lines.push(Line::from(spans));
            }

            // Last line: (indent)  [🔒]  repo  <pad>  updated (relative, right-aligned).
            // Indent by the fixed `ROW_INDENT_W` so repo lines up across rows (see its doc).
            let mut line2_spans = vec![Span::raw(" ".repeat(ROW_INDENT_W))];
            if item.repo_private {
                line2_spans.push(Span::styled(
                    format!("{} ", app.icons.private),
                    Style::default().fg(Color::Yellow),
                ));
            }
            line2_spans.push(Span::styled(repo, Style::default().fg(Color::Gray)));
            // Right-align the relative update time to the row's content edge.
            let upd_span = Span::styled(updated, Style::default().fg(Color::Gray));
            let used: usize = line2_spans.iter().map(Span::width).sum();
            let pad = inner_w.saturating_sub(used + upd_span.width()).max(1);
            line2_spans.push(Span::raw(" ".repeat(pad)));
            line2_spans.push(upd_span);
            lines.push(Line::from(line2_spans));

            ListItem::new(lines)
        })
        .collect();

    let mut state = ListState::default();
    if !filtered.is_empty() {
        state.select(Some(app.item_cursor));
    }

    let list = List::new(items)
        .block(block)
        .highlight_style(highlight_style(focused))
        .highlight_symbol(HIGHLIGHT_SYMBOL);

    f.render_stateful_widget(list, list_area, &mut state);
}

// ── Right pane: item detail ───────────────────────────────────────────────────

fn draw_item_detail(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::ItemDetail;
    let block = pane_block("Detail", focused);

    let text = match app.selected_item() {
        None => vec![Line::from(Span::raw("No item selected"))],
        Some(item) => {
            let repo = format!("{}/{}", item.repo_owner, item.repo_name);
            let author = item
                .author
                .as_ref()
                .map(|u| u.login.clone())
                .unwrap_or_else(|| "—".to_string());
            let state = item.state.clone();
            let title = item.title.clone();
            let updated_at = glauca_core::time::format_local_datetime(&item.updated_at);
            let created_at = item
                .created_at_item
                .as_deref()
                .map(glauca_core::time::format_local_datetime)
                .unwrap_or_else(|| "—".to_string());
            let url = item.url.clone();
            let number = item.number;
            let comment_count = item.comment_count;
            let item_icon = app.icons.item_icon(&item.kind, &state);
            let is_pr = item.kind == "pull_request";

            let labels = if item.labels.is_empty() {
                "—".to_string()
            } else {
                item.labels.join(", ")
            };
            let assignees = if item.assignees.is_empty() {
                "—".to_string()
            } else {
                item.assignees
                    .iter()
                    .map(|u| u.login.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let milestone = item.milestone.clone().unwrap_or_else(|| "—".to_string());

            // Build combined reviewer list: submitted reviews + pending requests
            let reviewed_logins: std::collections::HashSet<&str> =
                item.reviews.iter().map(|(u, _)| u.login.as_str()).collect();
            let reviewer_spans: Vec<Span> = {
                let mut spans = Vec::new();
                for (user, state) in &item.reviews {
                    let (badge, style) = app.icons.review_state_badge(state);
                    if !spans.is_empty() {
                        spans.push(Span::raw("  "));
                    }
                    spans.push(Span::styled(badge, style));
                    spans.push(Span::raw(format!(" {}", user.login)));
                }
                for user in &item.requested_reviewers {
                    if !reviewed_logins.contains(user.login.as_str()) {
                        if !spans.is_empty() {
                            spans.push(Span::raw("  "));
                        }
                        spans.push(Span::styled(
                            app.icons.pending_reviewer,
                            Style::default().fg(Color::Yellow),
                        ));
                        spans.push(Span::raw(format!(" {}", user.login)));
                    }
                }
                if spans.is_empty() {
                    spans.push(Span::raw("—"));
                }
                spans
            };

            let mut lines = vec![
                // Title header: combined kind+state icon (same as the list row),
                // coloured by state; number in cyan; title bold.
                Line::from(vec![
                    Span::styled(item_icon, state_style(&state)),
                    Span::styled(format!("  #{number} "), Style::default().fg(Color::Cyan)),
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
                    // State is shown by the header icon's colour; here just the
                    // word, coloured to match (the badge would be redundant).
                    let mut spans = vec![
                        Span::styled("State:    ", Style::default().fg(Color::Gray)),
                        Span::styled(state.clone(), state_style(&state)),
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
                    let (icon, style) = app.icons.review_decision_badge(rd);
                    let badge = match rd.as_str() {
                        "APPROVED" => format!("{icon} APPROVED"),
                        "CHANGES_REQUESTED" => format!("{icon} CHANGES REQUESTED"),
                        "REVIEW_REQUIRED" => format!("{icon} REVIEW REQUIRED"),
                        other => other.to_string(),
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
            if let Some(body) = &item.body
                && !body.is_empty()
            {
                lines.push(Line::default());
                lines.push(Line::from(Span::styled(
                    "─── Description ────────────────────────────────",
                    Style::default().fg(Color::DarkGray),
                )));
                lines.extend(tui_markdown::from_str(body).lines);
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

    let mode_text = match app.input_mode {
        InputMode::Normal => match app.focus {
            Focus::QueryList => "QUERIES  h/l:pane  j/k:move  J/K:reorder  n:new query  f:new stream  e:edit  d:delete  r:refresh  a:mark all read  ?:help  q:quit".to_string(),
            Focus::ItemList => format!("ITEMS    h/l:pane  j/k:move  /:filter{enter_actions_hint}{refresh_hint}{review_hint}  ?:help  q:quit"),
            Focus::ItemDetail => format!("DETAIL   h/l:pane  j/k:scroll{enter_actions_hint}{refresh_hint}{review_hint}  ?:help  q:quit"),
        },
        InputMode::Filter => "FILTER   Esc:exit  C-u:clear  is:pr  state:open  author:name  label:bug  repo:owner/name".to_string(),
        InputMode::NewQuery => "NEW QUERY  Tab:switch field  Enter:save  Esc:cancel".to_string(),
        InputMode::NewFilterStream => "NEW STREAM  Tab:switch field  Enter:save  Esc:cancel".to_string(),
        InputMode::EditQuery => "EDIT QUERY  Tab:switch field  Enter:save  Esc:cancel".to_string(),
        InputMode::EditFilterStream => "EDIT STREAM  Tab:switch field  Enter:save  Esc:cancel".to_string(),
        InputMode::ActionMenu => "ACTIONS  j/k:move  Enter:confirm  Esc:cancel".to_string(),
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

// ── New query modal ───────────────────────────────────────────────────────────

/// Draw a centered two-field input modal (a name field plus a second field).
/// `border_color` tints the border and whichever field is `active_field` (0 or
/// 1); the active field also shows a trailing `_` cursor. Shared by the
/// new/edit query and filter-stream modals, which differ only in these strings.
fn draw_two_field_modal(
    f: &mut Frame,
    area: Rect,
    title: &str,
    border_color: Color,
    fields: [(&str, &str); 2],
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

    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1); 5])
        .split(inner);

    for (i, (label, value)) in fields.iter().enumerate() {
        let active = active_field == i;
        let style = if active {
            Style::default().fg(border_color)
        } else {
            Style::default().fg(Color::Gray)
        };
        f.render_widget(Paragraph::new(*label), split[i * 2]);
        f.render_widget(
            Paragraph::new(format!("> {}{}", value, if active { "_" } else { "" })).style(style),
            split[i * 2 + 1],
        );
    }
    f.render_widget(
        Paragraph::new("Tab:switch  Enter:save  Esc:cancel")
            .style(Style::default().fg(Color::Gray)),
        split[4],
    );
}

fn draw_new_query_modal(f: &mut Frame, app: &App, area: Rect) {
    draw_two_field_modal(
        f,
        area,
        " New Query ",
        Color::Yellow,
        [
            (
                "Display name (optional — leave blank to use query):",
                &app.new_query_name,
            ),
            (
                "GitHub search query (e.g. repo:owner/name is:pr is:open):",
                &app.new_query_input,
            ),
        ],
        app.modal_field,
    );
}

fn draw_new_filter_stream_modal(f: &mut Frame, app: &App, area: Rect) {
    draw_two_field_modal(
        f,
        area,
        " New Filter Stream ",
        Color::Magenta,
        [
            ("Display name:", &app.new_filter_stream_name),
            (
                "Filter (e.g. is:pr is:draft assignee:name label:bug):",
                &app.new_filter_stream_filter,
            ),
        ],
        app.modal_field,
    );
}

fn draw_edit_query_modal(f: &mut Frame, app: &App, area: Rect) {
    draw_two_field_modal(
        f,
        area,
        " Edit Query ",
        Color::Cyan,
        [
            (
                "Display name (empty = use query string as label):",
                &app.edit_input,
            ),
            ("GitHub search query:", &app.edit_input2),
        ],
        app.modal_field,
    );
}

fn draw_edit_filter_stream_modal(f: &mut Frame, app: &App, area: Rect) {
    draw_two_field_modal(
        f,
        area,
        " Edit Filter Stream ",
        Color::Cyan,
        [
            ("Display name:", &app.edit_input),
            (
                "Filter (e.g. is:pr assignee:name milestone:v2 repo:owner/name):",
                &app.edit_input2,
            ),
        ],
        app.modal_field,
    );
}

fn draw_action_popup(f: &mut Frame, app: &App, area: Rect) {
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

/// Keybinding cheat-sheet overlay (opened with `?`). Static two-column list so it
/// fits without scrolling; closed with Esc / `?` / `q` (see `handle_key_help`).
fn draw_help_popup(f: &mut Frame, area: Rect) {
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

fn draw_review_menu_popup(f: &mut Frame, app: &App, area: Rect) {
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
        // Apply sort order
        let mut ordered: Vec<&CommentEntry> = app.comments.iter().collect();
        if app.comments_sort_desc {
            ordered.reverse();
        }
        let lines = build_comment_lines(
            &ordered,
            chunks[0].width as usize,
            app.comments_show_hidden,
            &app.icons,
        );
        let total = lines.len();
        // Clamp scroll
        let max_scroll = total.saturating_sub(chunks[0].height as usize);
        let scroll = app.comments_scroll.min(max_scroll);

        let para = Paragraph::new(lines)
            .scroll((scroll as u16, 0))
            .wrap(Wrap { trim: false });
        f.render_widget(para, chunks[0]);

        // Status / hint bar
        let hidden_count = app.comments.iter().filter(|c| c.is_minimized).count();
        let sort_label = if app.comments_sort_desc {
            "newest↑"
        } else {
            "oldest↑"
        };
        let hidden_toggle = if hidden_count > 0 {
            if app.comments_show_hidden {
                format!("h:hide({hidden_count})")
            } else {
                format!("h:show({hidden_count} hidden)")
            }
        } else {
            String::new()
        };
        let scroll_info = if total > chunks[0].height as usize {
            format!("{}/{}", scroll + chunks[0].height as usize, total)
        } else {
            String::new()
        };
        let hint = Paragraph::new(Line::from(vec![
            Span::styled(
                "Esc/q:close  j/k:scroll  g/G:top/bottom  ",
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("s:{sort_label}  "),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(hidden_toggle, Style::default().fg(Color::Yellow)),
            Span::styled(
                if scroll_info.is_empty() {
                    String::new()
                } else {
                    format!("  {scroll_info}")
                },
                Style::default().fg(Color::DarkGray),
            ),
        ]))
        .alignment(Alignment::Left);
        f.render_widget(hint, chunks[1]);
    }
}

fn build_comment_lines<'a>(
    comments: &[&'a CommentEntry],
    width: usize,
    show_hidden: bool,
    icons: &Icons,
) -> Vec<Line<'a>> {
    let mut lines: Vec<Line<'a>> = Vec::new();
    let sep_width = width.max(4) - 4; // account for block padding
    let mut first = true;
    for c in comments {
        if !first {
            lines.push(Line::from(Span::styled(
                "━".repeat(sep_width),
                Style::default().fg(Color::Yellow),
            )));
            lines.push(Line::from(""));
        }
        first = false;

        if c.is_minimized && !show_hidden {
            // Collapsed stub
            let reason = c.minimized_reason.as_deref().unwrap_or("hidden");
            lines.push(Line::from(vec![
                Span::styled("▸ ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("@{}", c.author),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ),
                Span::styled(
                    format!("  [hidden: {reason}]  — press h to expand"),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        } else {
            // Full comment
            let hidden_prefix = if c.is_minimized {
                vec![Line::from(Span::styled(
                    format!(
                        "  ⚠ This comment was hidden ({})",
                        c.minimized_reason.as_deref().unwrap_or("hidden")
                    ),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::ITALIC),
                ))]
            } else {
                vec![]
            };

            // Header line: ▌ @author  🕐 2026-05-24 20:15
            lines.push(Line::from(vec![
                Span::styled("▌ ", Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!("@{}", c.author),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if c.created_at.is_empty() {
                        String::new()
                    } else {
                        format!("   {} {}", icons.clock, c.created_at)
                    },
                    Style::default().fg(Color::White),
                ),
            ]));
            lines.extend(hidden_prefix);
            lines.push(Line::from(""));
            lines.extend(tui_markdown::from_str(&c.body).lines);
            lines.push(Line::from(""));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Concatenate a wrapped line's spans back into plain text.
    fn line_text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn line_width(spans: &[Span]) -> usize {
        line_text(spans)
            .chars()
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
            .sum()
    }

    #[test]
    fn wraps_on_word_boundaries() {
        let spans = vec![Span::raw("hello world foo bar")];
        let lines = wrap_spans(&spans, 11);
        let texts: Vec<String> = lines.iter().map(|l| line_text(l)).collect();
        assert_eq!(texts, vec!["hello world", "foo bar"]);
        // No line exceeds the limit.
        assert!(lines.iter().all(|l| line_width(l) <= 11));
    }

    #[test]
    fn hard_breaks_a_word_longer_than_width() {
        let spans = vec![Span::raw("supercalifragilistic")];
        let lines = wrap_spans(&spans, 5);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|l| line_width(l) <= 5));
        // Nothing is dropped.
        let joined: String = lines.iter().map(|l| line_text(l)).collect();
        assert_eq!(joined, "supercalifragilistic");
    }

    #[test]
    fn counts_full_width_chars_as_two_columns() {
        // Each CJK char is 2 columns wide; width 4 fits exactly two per line.
        let spans = vec![Span::raw("あいうえお")];
        let lines = wrap_spans(&spans, 4);
        let texts: Vec<String> = lines.iter().map(|l| line_text(l)).collect();
        assert_eq!(texts, vec!["あい", "うえ", "お"]);
        assert!(lines.iter().all(|l| line_width(l) <= 4));
    }

    #[test]
    fn preserves_per_fragment_styles() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let hl = Style::default().fg(Color::Black).bg(Color::Yellow);
        let spans = vec![Span::styled("foo ", bold), Span::styled("bar", hl)];
        let lines = wrap_spans(&spans, 100);
        assert_eq!(lines.len(), 1);
        // The highlighted "bar" keeps its own style as a distinct span.
        assert!(lines[0].iter().any(|s| s.content == "bar" && s.style == hl));
    }
}
