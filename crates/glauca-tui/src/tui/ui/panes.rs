//! Left pane (saved queries + filter streams) and middle pane (item list).

use super::*;

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

// ── Left pane: saved queries ─────────────────────────────────────────────────

pub(super) fn draw_query_list(f: &mut Frame, app: &App, area: Rect) {
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
                    let mut spans = vec![Span::raw(format!("{}  {}", app.icons.query, q.label))];
                    if let Some(badge) = badge {
                        spans.push(badge);
                    }
                    ListItem::new(Line::from(spans))
                }
                LeftPaneEntry::FilterStream(fs) => {
                    let mut spans = vec![Span::styled(
                        format!("   {}  {}", app.icons.filter_stream, fs.name),
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

    // Record the list interior (inside the border) for mouse hit-testing before
    // the block is moved into the List.
    let inner = block.inner(area);

    let list = List::new(items)
        .block(block)
        .highlight_style(highlight_style(focused))
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, area, &mut state);

    let mut regions = app.mouse_regions.borrow_mut();
    regions.query_inner = Some(inner);
    regions.query_offset = state.offset();
    regions.query_len = app.entries.len();
}

// ── Middle pane: item list ────────────────────────────────────────────────────

pub(super) fn draw_item_list(f: &mut Frame, app: &App, area: Rect) {
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
    let filter_block = pane_block("", filter_mode);
    if filter_mode {
        // Editable: draw the "/ " prompt, then the TextArea (which renders its
        // own cursor and scrolls horizontally when the text overflows).
        let inner = filter_block.inner(filter_area);
        f.render_widget(filter_block, filter_area);
        draw_prompted_field(
            f,
            inner,
            "/ ",
            Style::default().fg(Color::Yellow),
            &app.filter,
        );
    } else {
        let filter_label = if app.filter.is_empty() {
            " /:filter".to_string()
        } else {
            format!("/ {}  (Esc:exit  C-u:clear)", app.filter.value())
        };
        let filter_style = if !app.filter.is_empty() {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::Gray)
        };
        let filter_para = Paragraph::new(filter_label)
            .style(filter_style)
            .block(filter_block);
        f.render_widget(filter_para, filter_area);
    }

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

    // Rows outside this window get plain (unhighlighted) titles; see
    // `highlight_window` for why the window always covers the viewport.
    let hl_window = highlight_window(app.item_cursor, list_area.height);

    // Sample the clock once for the whole list so each row's relative time is
    // measured against the same instant (and we avoid a per-row `Utc::now()`).
    let now = Utc::now();
    // Per-row heights (rows are variable-height), recorded for mouse hit-testing.
    let mut row_heights: Vec<u16> = Vec::with_capacity(filtered.len());
    let items: Vec<ListItem> = filtered
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            let item_style = state_style(&item.state);
            let item_icon = app.icons.item_icon(&item.kind, &item.state);
            let repo = item.repo_display();
            let updated = glauca_core::time::format_relative_time_since(&item.updated_at, now);
            let title_spans = if hl_window.contains(&idx) {
                highlight_spans(&filter_query, &item.title, match_normal, match_highlight)
            } else {
                vec![Span::styled(item.title.as_str(), match_normal)]
            };

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

            row_heights.push(lines.len() as u16);
            ListItem::new(lines)
        })
        .collect();

    let mut state = ListState::default();
    if !filtered.is_empty() {
        state.select(Some(app.item_cursor));
    }

    // List interior (inside the border) for mouse hit-testing, captured before
    // the block is moved into the List.
    let inner = block.inner(list_area);

    let list = List::new(items)
        .block(block)
        .highlight_style(highlight_style(focused))
        .highlight_symbol(HIGHLIGHT_SYMBOL);

    f.render_stateful_widget(list, list_area, &mut state);

    let mut regions = app.mouse_regions.borrow_mut();
    regions.item_inner = Some(inner);
    regions.item_offset = state.offset();
    regions.item_heights = row_heights;
}
