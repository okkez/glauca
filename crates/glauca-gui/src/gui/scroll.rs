//! Scroll-offset math and the shared pane frame. `clamp_scroll_y` is split
//! out so the bounds logic is unit-testable without a laid-out `ScrollHandle`.

use gpui::*;
use gpui_component::{ActiveTheme, v_flex};

/// Pixels scrolled per j/k keypress in the detail pane and comments overlay.
pub(crate) const DETAIL_SCROLL_STEP: f32 = 48.0;

/// Clamp a new vertical scroll offset into gpui's valid range. gpui offsets go
/// negative downward: `0` is the top and `-max_offset_y` is the bottom, so a
/// positive `delta_px` (scroll down) subtracts. Split out from `scroll_vertically`
/// so the bounds logic is unit-testable without a laid-out `ScrollHandle` (whose
/// `max_offset` is only known after a real layout pass).
fn clamp_scroll_y(current_y: Pixels, delta_px: f32, max_offset_y: Pixels) -> Pixels {
    // `-max_offset_y <= 0`, so the bounds are always ordered and `clamp` can't panic.
    (current_y - px(delta_px)).clamp(-max_offset_y, px(0.))
}

/// Scroll a tracked `overflow_y_scroll` container by `delta_px` pixels (positive
/// = down), clamped to the content via [`clamp_scroll_y`].
pub(crate) fn scroll_vertically(handle: &ScrollHandle, delta_px: f32) {
    let mut off = handle.offset();
    off.y = clamp_scroll_y(off.y, delta_px, handle.max_offset().y);
    handle.set_offset(off);
}

/// Wrap a pane's `content` in the standard frame: a neutral 1px border on the
/// left/right/bottom edges plus a top edge that turns `primary` when `focused`
/// — the keyboard-focus indicator. gpui colors a border uniformly, so the two
/// colors are split across an outer (top) element and an inner (other three).
pub(crate) fn pane_frame(focused: bool, content: impl IntoElement, cx: &App) -> Div {
    let top = if focused {
        cx.theme().primary
    } else {
        cx.theme().border
    };
    v_flex().size_full().border_t_1().border_color(top).child(
        v_flex()
            .size_full()
            .border_l_1()
            .border_r_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(content),
    )
}

#[cfg(test)]
mod tests {
    use super::{DETAIL_SCROLL_STEP, clamp_scroll_y};
    use gpui::px;

    #[test]
    fn scroll_down_within_bounds_moves_offset_negative() {
        // From the top, one step down: offset goes negative by the step.
        assert_eq!(
            clamp_scroll_y(px(0.), DETAIL_SCROLL_STEP, px(200.)),
            px(-DETAIL_SCROLL_STEP)
        );
    }

    #[test]
    fn scroll_down_past_bottom_clamps_to_max() {
        // Near the bottom, a further step down is clamped to -max_offset.
        assert_eq!(clamp_scroll_y(px(-180.), 48., px(200.)), px(-200.));
    }

    #[test]
    fn scroll_up_past_top_clamps_to_zero() {
        // Just below the top, scrolling up (negative delta) can't exceed 0.
        assert_eq!(clamp_scroll_y(px(-20.), -48., px(200.)), px(0.));
    }

    #[test]
    fn content_shorter_than_viewport_stays_pinned_at_top() {
        // max_offset == 0 (content fits): every scroll stays at the top.
        assert_eq!(clamp_scroll_y(px(0.), 48., px(0.)), px(0.));
        assert_eq!(clamp_scroll_y(px(0.), -48., px(0.)), px(0.));
    }
}
