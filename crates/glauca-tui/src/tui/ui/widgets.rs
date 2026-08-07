//! Small shared rendering helpers: pane blocks, styles, centered popup rects,
//! the item-list highlight window, and the prompt+field row.

use super::*;

/// Item-index window guaranteed to cover everything ratatui can display for a list of the
/// given height, used to limit the per-row fuzzy highlight — a Smith-Waterman scan, too
/// costly for every item in a 1000+ list.
///
/// ratatui measures the variable row heights and picks the scroll offset at render time,
/// after the rows are built, so the exact visible range is unknowable here. This
/// over-approximates: at most `list_height` rows fit, since each is at least one line, so
/// `list_height` items either side of the cursor always contain the viewport however the
/// titles wrap. Off-window rows are never on screen, so skipping them is invisible.
pub(super) fn highlight_window(cursor: usize, list_height: u16) -> std::ops::Range<usize> {
    let radius = list_height as usize;
    cursor.saturating_sub(radius)..cursor.saturating_add(radius + 1)
}

pub(super) fn pane_block(title: &str, focused: bool) -> Block<'_> {
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

pub(super) fn highlight_style(focused: bool) -> Style {
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

pub(super) fn state_style(state: &str) -> Style {
    match state {
        "open" => Style::default().fg(Color::Green),
        "merged" => Style::default().fg(Color::Magenta),
        "closed" => Style::default().fg(Color::Red),
        _ => Style::default(),
    }
}

/// Returns a centered `Rect` of fixed height and `percent_x` width.
pub(super) fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let popup_width = area.width * percent_x / 100;
    let x = area.x + (area.width.saturating_sub(popup_width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, popup_width.min(area.width), height.min(area.height))
}

pub(super) fn centered_rect_fixed(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

/// Draw a fixed 2-column `prompt` followed by the editable `ta` in the remaining width.
/// The cell is 2 columns, so `prompt` must be 2 display columns (e.g. `"> "`). `ta` renders
/// its own cursor and scrolls horizontally on overflow. Shared by the modal fields and the
/// filter bar so the prompt width lives in one place.
pub(super) fn draw_prompted_field(
    f: &mut Frame,
    area: Rect,
    prompt: &str,
    prompt_style: Style,
    ta: &SingleLineInput,
) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(2), Constraint::Min(0)])
        .split(area);
    f.render_widget(Paragraph::new(prompt).style(prompt_style), cols[0]);
    f.render_widget(ta, cols[1]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_window_covers_cursor_and_clamps() {
        // Window is [cursor - height, cursor + height + 1), always containing the
        // cursor, and clamped at 0.
        let w = highlight_window(50, 10);
        assert_eq!(w, 40..61);
        assert!(w.contains(&50));
        // Clamps to 0 near the top rather than underflowing.
        assert_eq!(highlight_window(3, 10), 0..14);
        // A zero-height pane still yields the cursor row itself.
        assert_eq!(highlight_window(7, 0), 7..8);
    }
}
