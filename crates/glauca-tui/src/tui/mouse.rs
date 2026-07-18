//! Mouse event handling: hit-testing click/scroll coordinates against the
//! last-rendered pane regions and translating them into the same `Action`s and
//! cursor moves that `handle_key_normal` produces. The `MouseRegions` here are
//! populated by the `ui` draw functions each frame (layout is otherwise never
//! stored), so this module is the read side of that render-time bookkeeping.

use super::*;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use std::time::{Duration, Instant};

/// Two left-clicks on the same target within this window count as a double-click.
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// Rendered pane geometry captured during `ui::draw`, used to map a mouse
/// coordinate back to a pane/row. The item list has variable-height rows, so it
/// needs the per-row heights plus the list's scroll offset (both recorded after
/// the stateful render) to reconstruct which row a `y` falls in.
#[derive(Default)]
pub(crate) struct MouseRegions {
    /// Left pane whole column. A click here that misses a row still focuses the
    /// pane (`QueryPane`).
    pub(crate) query_col: Option<Rect>,
    /// Left pane list interior (inside the border). Rows are always 1 line tall.
    pub(crate) query_inner: Option<Rect>,
    /// First visible entry index (`ListState::offset`) of the left pane.
    pub(crate) query_offset: usize,
    /// Number of entries in the left pane (bounds the hit-test).
    pub(crate) query_len: usize,
    /// Middle pane whole column. A click on its filter bar / banner / border
    /// (outside `item_inner`) still focuses the pane (`ItemPane`).
    pub(crate) item_col: Option<Rect>,
    /// Middle pane item-list interior (inside the border).
    pub(crate) item_inner: Option<Rect>,
    /// First visible item index (`ListState::offset`) of the middle pane.
    pub(crate) item_offset: usize,
    /// Height, in lines, of every filtered item row (variable per row).
    pub(crate) item_heights: Vec<u16>,
    /// Right pane detail column (whole column, borders included).
    pub(crate) detail_area: Option<Rect>,
}

/// What a mouse coordinate resolves to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum MouseTarget {
    /// A concrete entry row in the left pane (`entries` index).
    QueryEntry(usize),
    /// The left pane, but not on any entry row.
    QueryPane,
    /// A concrete item row in the middle pane (`filtered_items` index).
    Item(usize),
    /// The middle pane, but not on any item row.
    ItemPane,
    /// The right (detail) pane.
    Detail,
    /// Outside every pane (borders/status bar/gaps).
    None,
}

/// Resolve a terminal coordinate to a [`MouseTarget`] using the last frame's
/// [`MouseRegions`]. Pure so it can be unit-tested without an `App`.
pub(crate) fn hit_test(regions: &MouseRegions, col: u16, row: u16) -> MouseTarget {
    let pos = Position::new(col, row);

    if let Some(area) = regions.item_inner
        && area.contains(pos)
    {
        // Walk visible rows from the scroll offset, summing heights, to find the
        // row whose vertical span contains `row`.
        let mut y = area.y;
        for (i, h) in regions
            .item_heights
            .iter()
            .enumerate()
            .skip(regions.item_offset)
        {
            let next = y.saturating_add(*h);
            if row < next {
                return MouseTarget::Item(i);
            }
            y = next;
            if y >= area.bottom() {
                break;
            }
        }
        return MouseTarget::ItemPane;
    }

    if let Some(area) = regions.query_inner
        && area.contains(pos)
    {
        let idx = regions.query_offset + (row - area.y) as usize;
        if idx < regions.query_len {
            return MouseTarget::QueryEntry(idx);
        }
        return MouseTarget::QueryPane;
    }

    // Pane-level fallbacks: a click inside a column but not on a row (filter bar,
    // banner, border) still focuses that pane.
    let hits = |area: Option<Rect>| area.is_some_and(|a| a.contains(pos));
    if hits(regions.item_col) {
        return MouseTarget::ItemPane;
    }
    if hits(regions.query_col) {
        return MouseTarget::QueryPane;
    }
    if hits(regions.detail_area) {
        return MouseTarget::Detail;
    }

    MouseTarget::None
}

/// Translate a mouse event into focus/cursor changes plus an [`Action`] for the
/// run loop to carry out (mirrors `handle_key_normal`). Returns `None` for events
/// we don't act on (motion, drag, button-up, non-left buttons) so the run loop
/// can skip an otherwise-wasted redraw — mouse capture reports motion events,
/// which would otherwise repaint the whole UI on every pointer move.
pub(crate) fn handle_mouse(app: &mut App, me: MouseEvent) -> Option<Action> {
    match me.kind {
        MouseEventKind::Down(MouseButton::Left) => Some(on_left_down(app, me.column, me.row)),
        MouseEventKind::ScrollDown => Some(on_scroll(app, me.column, me.row, true)),
        MouseEventKind::ScrollUp => Some(on_scroll(app, me.column, me.row, false)),
        _ => None,
    }
}

fn on_left_down(app: &mut App, col: u16, row: u16) -> Action {
    let target = hit_test(&app.mouse_regions.borrow(), col, row);

    // Double-click = a second press on the same target within the window. Reset
    // the window once it fires so a third rapid click starts a fresh pair (rather
    // than counting as yet another double-click).
    let is_double_click = matches!(
        app.last_mouse_click,
        Some((at, prev)) if prev == target && at.elapsed() < DOUBLE_CLICK_WINDOW
    );
    app.last_mouse_click = if is_double_click {
        None
    } else {
        Some((Instant::now(), target))
    };

    match target {
        MouseTarget::QueryEntry(i) => {
            app.focus = Focus::QueryList;
            app.entry_cursor = i;
            // The first press already loaded (and force-synced) this entry; the
            // second press of a double-click would only repeat that fetch.
            if is_double_click {
                Action::None
            } else {
                Action::LoadEntry
            }
        }
        MouseTarget::QueryPane => {
            app.focus = Focus::QueryList;
            Action::None
        }
        MouseTarget::Item(i) => {
            app.focus = Focus::ItemList;
            app.item_cursor = i;
            app.clamp_item_cursor();
            app.detail_scroll = 0;
            // Double-click opens the item in the browser (same as the `o` key).
            if is_double_click {
                Action::OpenBrowser
            } else {
                Action::None
            }
        }
        MouseTarget::ItemPane => {
            app.focus = Focus::ItemList;
            Action::None
        }
        MouseTarget::Detail => {
            app.focus = Focus::ItemDetail;
            Action::None
        }
        MouseTarget::None => Action::None,
    }
}

/// Scroll the pane under the cursor by one notch, mirroring `j`/`k`. Focus moves
/// to the scrolled pane so the run loop's item-focus post-processing (mark-read /
/// lazy body fetch) runs, matching keyboard navigation.
fn on_scroll(app: &mut App, col: u16, row: u16, down: bool) -> Action {
    let target = hit_test(&app.mouse_regions.borrow(), col, row);
    match target {
        MouseTarget::QueryEntry(_) | MouseTarget::QueryPane => {
            app.focus = Focus::QueryList;
            // Cached load (no forced sync): a scroll gesture emits several notches
            // and forcing a GitHub sync on each would burst API calls.
            if down {
                if app.entry_cursor + 1 < app.entries.len() {
                    app.entry_cursor += 1;
                    return Action::LoadEntryCached;
                }
            } else if app.entry_cursor > 0 {
                app.entry_cursor -= 1;
                return Action::LoadEntryCached;
            }
            Action::None
        }
        MouseTarget::Item(_) | MouseTarget::ItemPane => {
            app.focus = Focus::ItemList;
            let max = app.filtered_items().len().saturating_sub(1);
            if down {
                if app.item_cursor < max {
                    app.item_cursor += 1;
                    app.detail_scroll = 0;
                }
            } else if app.item_cursor > 0 {
                app.item_cursor -= 1;
                app.detail_scroll = 0;
            }
            Action::None
        }
        MouseTarget::Detail => {
            app.focus = Focus::ItemDetail;
            app.detail_scroll = if down {
                app.detail_scroll.saturating_add(1)
            } else {
                app.detail_scroll.saturating_sub(1)
            };
            Action::None
        }
        MouseTarget::None => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::make_app_with_items;
    use crossterm::event::KeyModifiers;

    fn one_query() -> Vec<QueryEntry> {
        vec![QueryEntry {
            id: 1,
            label: "a".into(),
            query_str: "a".into(),
            kind: "pull_request".into(),
        }]
    }

    fn two_queries() -> Vec<QueryEntry> {
        vec![
            QueryEntry {
                id: 1,
                label: "a".into(),
                query_str: "a".into(),
                kind: "pull_request".into(),
            },
            QueryEntry {
                id: 2,
                label: "b".into(),
                query_str: "b".into(),
                kind: "pull_request".into(),
            },
        ]
    }

    fn regions() -> MouseRegions {
        MouseRegions {
            // Left pane column x:0..20, interior x:1..19, y:1..10 (3 entries).
            query_col: Some(Rect::new(0, 0, 20, 11)),
            query_inner: Some(Rect::new(1, 1, 18, 9)),
            query_offset: 0,
            query_len: 3,
            // Middle pane column x:20..50, list interior x:21..49, y:1..10.
            item_col: Some(Rect::new(20, 0, 30, 11)),
            item_inner: Some(Rect::new(21, 1, 28, 9)),
            item_offset: 0,
            // 3 rows of heights 3, 3, 2 lines.
            item_heights: vec![3, 3, 2],
            // Right pane whole column.
            detail_area: Some(Rect::new(50, 0, 40, 11)),
        }
    }

    fn mouse(kind: MouseEventKind, col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn hit_test_maps_query_rows() {
        let r = regions();
        assert_eq!(hit_test(&r, 5, 1), MouseTarget::QueryEntry(0));
        assert_eq!(hit_test(&r, 5, 3), MouseTarget::QueryEntry(2));
        // Row 4 is past the 3 entries → pane, not an entry.
        assert_eq!(hit_test(&r, 5, 4), MouseTarget::QueryPane);
    }

    #[test]
    fn hit_test_query_respects_offset() {
        let mut r = regions();
        r.query_offset = 1;
        // Top visible row now shows entry index 1.
        assert_eq!(hit_test(&r, 5, 1), MouseTarget::QueryEntry(1));
    }

    #[test]
    fn hit_test_maps_variable_height_item_rows() {
        let r = regions();
        // Row 0 spans y 1..4, row 1 spans 4..7, row 2 spans 7..9.
        assert_eq!(hit_test(&r, 25, 1), MouseTarget::Item(0));
        assert_eq!(hit_test(&r, 25, 3), MouseTarget::Item(0));
        assert_eq!(hit_test(&r, 25, 4), MouseTarget::Item(1));
        assert_eq!(hit_test(&r, 25, 6), MouseTarget::Item(1));
        assert_eq!(hit_test(&r, 25, 7), MouseTarget::Item(2));
        assert_eq!(hit_test(&r, 25, 8), MouseTarget::Item(2));
    }

    #[test]
    fn hit_test_item_offset_shifts_rows() {
        let mut r = regions();
        r.item_offset = 1;
        // Now row 1 (height 3) renders at the top: y 1..4.
        assert_eq!(hit_test(&r, 25, 1), MouseTarget::Item(1));
        assert_eq!(hit_test(&r, 25, 4), MouseTarget::Item(2));
    }

    #[test]
    fn hit_test_detail_and_outside() {
        let r = regions();
        assert_eq!(hit_test(&r, 60, 5), MouseTarget::Detail);
        // Below the columns (e.g. the status-bar row) maps to nothing.
        assert_eq!(hit_test(&r, 5, 15), MouseTarget::None);
    }

    #[test]
    fn hit_test_pane_fallback_off_row() {
        let r = regions();
        // Inside the middle column but above the item list (filter bar/banner)
        // still focuses the item pane instead of falling through to None.
        assert_eq!(hit_test(&r, 25, 0), MouseTarget::ItemPane);
        // Inside the left column but off any entry row focuses the query pane.
        assert_eq!(hit_test(&r, 5, 0), MouseTarget::QueryPane);
    }

    #[test]
    fn hit_test_blank_area_below_last_item_is_pane() {
        // Rows occupy y 1..9 (heights 3+3+2); a click on the blank y=9 inside the
        // list interior falls through to ItemPane, not an item.
        let r = regions();
        assert_eq!(hit_test(&r, 25, 9), MouseTarget::ItemPane);
    }

    #[test]
    fn left_click_query_entry_selects_and_loads() {
        let mut app = App::new(two_queries());
        *app.mouse_regions.borrow_mut() = MouseRegions {
            query_inner: Some(Rect::new(1, 1, 18, 9)),
            query_len: 2,
            ..Default::default()
        };
        let action = handle_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), 5, 2),
        );
        assert!(matches!(action, Some(Action::LoadEntry)));
        assert_eq!(app.entry_cursor, 1);
        assert_eq!(app.focus, Focus::QueryList);
    }

    #[test]
    fn double_click_item_opens_browser() {
        let mut app = make_app_with_items(&["first", "second"]);
        *app.mouse_regions.borrow_mut() = MouseRegions {
            item_inner: Some(Rect::new(21, 1, 28, 9)),
            item_heights: vec![2, 2],
            ..Default::default()
        };
        let ev = || mouse(MouseEventKind::Down(MouseButton::Left), 25, 1);
        // First click just selects.
        assert!(matches!(handle_mouse(&mut app, ev()), Some(Action::None)));
        assert_eq!(app.focus, Focus::ItemList);
        // Second click on the same target within the window opens the browser.
        assert!(matches!(
            handle_mouse(&mut app, ev()),
            Some(Action::OpenBrowser)
        ));
        // Third rapid click starts a fresh pair — it must NOT re-open the browser.
        assert!(matches!(handle_mouse(&mut app, ev()), Some(Action::None)));
    }

    #[test]
    fn double_click_query_entry_does_not_resync() {
        let mut app = App::new(two_queries());
        *app.mouse_regions.borrow_mut() = MouseRegions {
            query_inner: Some(Rect::new(1, 1, 18, 9)),
            query_len: 2,
            ..Default::default()
        };
        let ev = || mouse(MouseEventKind::Down(MouseButton::Left), 5, 1);
        // First press loads (force-sync); the double-click second press must not
        // repeat the forced GitHub fetch.
        assert!(matches!(
            handle_mouse(&mut app, ev()),
            Some(Action::LoadEntry)
        ));
        assert!(matches!(handle_mouse(&mut app, ev()), Some(Action::None)));
    }

    #[test]
    fn scroll_focuses_the_scrolled_pane() {
        let mut app = make_app_with_items(&["a", "b"]);
        *app.mouse_regions.borrow_mut() = MouseRegions {
            item_inner: Some(Rect::new(21, 1, 28, 9)),
            item_heights: vec![1, 1],
            detail_area: Some(Rect::new(50, 0, 40, 11)),
            ..Default::default()
        };
        app.focus = Focus::QueryList;
        // Wheel over the item list focuses it, so the run loop's item-focus
        // post-processing (mark-read) runs.
        handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown, 25, 1));
        assert_eq!(app.focus, Focus::ItemList);
        // Wheel over the detail pane focuses it.
        handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown, 60, 5));
        assert_eq!(app.focus, Focus::ItemDetail);
    }

    #[test]
    fn draw_populates_regions_and_clicks_hit() {
        use ratatui::{Terminal, backend::TestBackend};

        let app = make_app_with_items(&["first item", "second item", "third item"]);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| crate::tui::ui::draw(f, &app)).unwrap();

        let r = app.mouse_regions.borrow();
        let item_inner = r.item_inner.expect("item region populated");
        let query_inner = r.query_inner.expect("query region populated");
        let detail = r.detail_area.expect("detail region populated");
        assert_eq!(r.item_heights.len(), 3);

        // Top-left of each interior maps to its first row / pane.
        assert_eq!(
            hit_test(&r, item_inner.x, item_inner.y),
            MouseTarget::Item(0)
        );
        assert_eq!(
            hit_test(&r, query_inner.x, query_inner.y),
            MouseTarget::QueryEntry(0)
        );
        assert_eq!(
            hit_test(&r, detail.x + 1, detail.y + 1),
            MouseTarget::Detail
        );
    }

    #[test]
    fn scroll_detail_adjusts_scroll_offset() {
        let mut app = App::new(one_query());
        *app.mouse_regions.borrow_mut() = MouseRegions {
            detail_area: Some(Rect::new(50, 0, 40, 11)),
            ..Default::default()
        };
        handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown, 60, 5));
        assert_eq!(app.detail_scroll, 1);
        handle_mouse(&mut app, mouse(MouseEventKind::ScrollUp, 60, 5));
        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn scroll_query_pane_uses_cached_load() {
        let mut app = App::new(two_queries());
        *app.mouse_regions.borrow_mut() = MouseRegions {
            query_inner: Some(Rect::new(1, 1, 18, 9)),
            query_len: 2,
            ..Default::default()
        };
        // Wheel over the query pane must not force a sync (LoadEntryCached, not
        // LoadEntry) to avoid a burst of GitHub fetches per scroll gesture.
        let action = handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown, 5, 1));
        assert!(matches!(action, Some(Action::LoadEntryCached)));
        assert_eq!(app.entry_cursor, 1);
    }

    #[test]
    fn motion_events_are_ignored() {
        let mut app = App::new(one_query());
        *app.mouse_regions.borrow_mut() = MouseRegions {
            detail_area: Some(Rect::new(50, 0, 40, 11)),
            ..Default::default()
        };
        // Pointer motion must return None so the run loop can skip its redraw.
        assert!(handle_mouse(&mut app, mouse(MouseEventKind::Moved, 60, 5)).is_none());
    }
}
