//! Comments overlay: the popup and its comment-to-line rendering.

use super::*;

pub(super) fn draw_comments_popup(f: &mut Frame, app: &App, area: Rect) {
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
