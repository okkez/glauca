//! Detail pane and comments-overlay rendering (the two largest read views).

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::scroll::ScrollableElement;
use gpui_component::text::{TextView, markdown};
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme, StyledExt, h_flex, v_flex};

use super::*;

impl GlaucaApp {
    pub(crate) fn render_detail(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        // The pane frame is added by the caller via `pane_frame`, so the early returns
        // below don't each have to wrap themselves.
        let container = v_flex()
            .id("detail-pane")
            .size_full()
            .bg(cx.theme().background)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    if let Some(item) = this.selected_item() {
                        this.open_menu(ev.position, MenuKind::Item(Box::new(item)), window, cx);
                    }
                }),
            );

        let Some(item) = self
            .filtered
            .get(self.item_cursor)
            .and_then(|&i| self.items.get(i))
        else {
            return container.child(
                div()
                    .p_4()
                    .text_color(cx.theme().muted_foreground)
                    .child("Select an item"),
            );
        };

        let (state_path, state_color) = item_state_icon_info(item, cx);

        // Pinned header: metadata stays visible while the body scrolls. It cannot share a
        // scroll region with the body, which owns its own, so it is a `flex_none` block.
        let header = v_flex()
            .flex_none()
            .p_4()
            .gap_2()
            // Title line: author avatar + state pill + (PR) review-decision icon +
            // the item title (which wraps; the leading items stay on the top line).
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_start()
                    // A vertically-centered cluster so the avatar, state pill and review
                    // icon line up, with the cluster on the title's first line.
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .flex_shrink_0()
                            .when_some(item.author.clone(), |e, a| {
                                let login = a.login.clone();
                                e.child(
                                    div()
                                        .id("detail-author")
                                        .flex_shrink_0()
                                        .child(user_avatar(&a))
                                        .tooltip(move |window, cx| {
                                            Tooltip::new(login.clone()).build(window, cx)
                                        }),
                                )
                            })
                            // GitHub-style state pill: colored, rounded, light text.
                            .child(
                                h_flex()
                                    .gap_1()
                                    .items_center()
                                    .flex_shrink_0()
                                    .px_2()
                                    .py_0p5()
                                    .rounded_full()
                                    .bg(state_color)
                                    .text_color(white())
                                    .child(
                                        svg()
                                            .path(state_path)
                                            .size_3()
                                            .flex_shrink_0()
                                            .text_color(white()),
                                    )
                                    .child(div().text_xs().child(state_label(item))),
                            )
                            // Review decision as an icon with a tooltip (PRs only).
                            .when_some(item.review_decision.clone(), |e, decision| {
                                let (icon, color, label) = review_decision_icon(&decision, cx);
                                e.child(
                                    div()
                                        .id("review-decision")
                                        .flex_shrink_0()
                                        .child(
                                            svg()
                                                .path(icon)
                                                .size_5()
                                                .flex_shrink_0()
                                                .text_color(color),
                                        )
                                        .tooltip(move |window, cx| {
                                            Tooltip::new(label).build(window, cx)
                                        }),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_lg()
                            .font_bold()
                            .text_color(cx.theme().foreground)
                            .child(SharedString::from(item.title.clone())),
                    ),
            )
            .when(!item.labels.is_empty(), |e| {
                e.child(detail_field("labels", &item.labels.join(", "), cx))
            })
            .when_some(
                item.base_ref.as_ref().zip(item.head_ref.as_ref()),
                |e, (base, head)| e.child(detail_field("branch", &format!("{head} → {base}"), cx)),
            )
            .when(!item.assignees.is_empty(), |e| {
                e.child(detail_people_field(
                    "assignees",
                    item.assignees.iter().cloned().map(|u| user_chip(u, cx)),
                    cx,
                ))
            })
            .map(|e| {
                // Unified reviewers row: requested ∪ reviewed, state shown by the overlay.
                let reviewers = reviewer_overlays(item);
                e.when(!reviewers.is_empty(), |e| {
                    e.child(detail_people_field(
                        "reviewers",
                        reviewers.into_iter().map(|(u, s)| reviewer_chip(u, s, cx)),
                        cx,
                    ))
                })
            })
            .when_some(item.milestone.as_ref(), |e, m| {
                e.child(detail_field("milestone", m, cx))
            })
            .when_some(item.created_at_item.as_deref(), |e, created| {
                e.child(detail_field(
                    "created",
                    &glauca_core::time::format_local_datetime(created),
                    cx,
                ))
            })
            .child(detail_field(
                "updated",
                &glauca_core::time::format_local_datetime(&item.updated_at),
                cx,
            ));

        // Markdown via `TextView` inside a tracked `overflow_y_scroll` container rather
        // than the TextView's own `scrollable(true)`, whose ListState is private to
        // gpui-component and would leave j/k with nothing to drive. The parse is retained
        // in `detail_text`; `set_text` is a no-op unless the body actually changed.
        let body = match item
            .body
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(body) => {
                self.detail_text
                    .update(cx, |state, cx| state.set_text(body, cx));
                div()
                    .flex_1()
                    .min_h_0()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .relative()
                    .child(
                        div()
                            .id("detail-scroll")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.detail_scroll)
                            .px_4()
                            .pb_4()
                            .pt_2()
                            .child(TextView::new(&self.detail_text).selectable(true)),
                    )
                    .vertical_scrollbar(&self.detail_scroll)
                    .into_any_element()
            }
            None => div()
                .flex_none()
                .px_4()
                .pb_4()
                .pt_2()
                .border_t_1()
                .border_color(cx.theme().border)
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("(no description)")
                .into_any_element(),
        };

        container.child(header).child(body)
    }

    /// Self-managed comments overlay, rendered over the panes when `comments_open` and
    /// repainted when `CommentsLoaded` arrives. Keys are scoped to `COMMENTS_CONTEXT`.
    pub(crate) fn render_comments_overlay(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let hidden_count = self.comments.iter().filter(|c| c.is_minimized).count();
        let sort_label = if self.comments_sort_desc {
            "newest"
        } else {
            "oldest"
        };

        let body = if self.comments_loading {
            div()
                .p_4()
                .text_color(cx.theme().muted_foreground)
                .child("Loading comments…")
                .into_any_element()
        } else if self.comments.is_empty() {
            div()
                .p_4()
                .text_color(cx.theme().muted_foreground)
                .child("No comments.")
                .into_any_element()
        } else {
            let mut order: Vec<usize> = (0..self.comments.len()).collect();
            if self.comments_sort_desc {
                order.reverse();
            }
            let mut list = v_flex()
                .id("comments-scroll")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.comments_scroll)
                .p_3()
                .gap_3();
            for (pos, idx) in order.into_iter().enumerate() {
                let c = &self.comments[idx];
                let head = h_flex()
                    .gap_2()
                    .text_sm()
                    .child(
                        div()
                            .font_bold()
                            .text_color(cx.theme().foreground)
                            .child(SharedString::from(format!("@{}", c.author))),
                    )
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child(SharedString::from(c.created_at.clone())),
                    );
                let mut block = v_flex().gap_1().child(head);
                if c.is_minimized && !self.comments_show_hidden {
                    let reason = c
                        .minimized_reason
                        .clone()
                        .unwrap_or_else(|| "minimized".into());
                    block = block.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(SharedString::from(format!(
                                "▸ hidden ({reason}) — press h to expand"
                            ))),
                    );
                } else {
                    if c.is_minimized {
                        let reason = c
                            .minimized_reason
                            .clone()
                            .unwrap_or_else(|| "minimized".into());
                        block = block.child(
                            div()
                                .text_xs()
                                // `yellow`, not `accent` — accent is a muted grey.
                                .text_color(cx.theme().yellow)
                                .child(SharedString::from(format!("⚠ hidden ({reason})"))),
                        );
                    }
                    block = block.child(markdown(c.body.clone()).selectable(true));
                }
                // Separator above every comment except the first.
                if pos > 0 {
                    block = block.pt_3().border_t_1().border_color(cx.theme().border);
                }
                list = list.child(block);
            }
            list.into_any_element()
        };

        let footer = format!(
            "Esc/q: close   j/k: scroll   g/G: top/bottom   s: {sort_label}   h: show/hide ({hidden_count})"
        );

        // Scrim: full-size, centers the panel, and swallows clicks to the panes.
        h_flex()
            .absolute()
            .inset_0()
            .p_8()
            .justify_center()
            .items_center()
            .bg(hsla(0., 0., 0., 0.5))
            .occlude()
            .child(
                v_flex()
                    .id("comments-overlay")
                    .track_focus(&self.comments_focus_handle)
                    .key_context(COMMENTS_CONTEXT)
                    .w(px(640.))
                    // Definite height (fills the scrim minus its padding) so the
                    // flex_1 body has room to expand and scroll. With only `max_h`
                    // the panel collapsed to its content height (tiny popup).
                    .h_full()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded_lg()
                    .shadow_lg()
                    .child(
                        div()
                            .w_full()
                            .flex_shrink_0()
                            .px_4()
                            .py_2()
                            .border_b_1()
                            .border_color(cx.theme().border)
                            .font_bold()
                            .text_color(cx.theme().foreground)
                            .child(self.comments_title.clone()),
                    )
                    .child(div().flex_1().min_h_0().child(body))
                    .child(
                        div()
                            .w_full()
                            .flex_shrink_0()
                            .px_4()
                            .py_2()
                            .border_t_1()
                            .border_color(cx.theme().border)
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(SharedString::from(footer)),
                    ),
            )
    }
}
